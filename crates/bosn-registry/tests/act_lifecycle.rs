use bosn_registry::{Registry, act::*};
use kernal_api::platform::fs::TemporaryDirectory;
use std::collections::BTreeMap;
const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const CLAIM: &str = "12345678-1234-4234-8234-123456789abc";
const RUN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
fn profile() -> ActEngineCreationProfile {
    ActEngineCreationProfile {
        memory_bytes: 28 << 30,
        storage_bytes: 20 << 30,
        nano_cpus: 2_000_000_000,
        pids: 1024,
        run_tmpfs_bytes: 16 << 20,
        tmp_tmpfs_bytes: 64 << 20,
        tmpfs_policy: ActEngineTmpfsPolicy::StorageExecRunTmpNoexecV1,
        init_command_sha256: "a".repeat(64),
    }
}
fn intent() -> ActEngineIntent {
    ActEngineIntent {
        run_id: RUN.into(),
        workspace: "/private/source".into(),
        candidate_sha: "a".repeat(40),
        payload_sha256: "b".repeat(64),
        snapshot_sha256: "c".repeat(64),
        act_version: "0.2.88".into(),
        act_image_digest: format!("sha256:{}", "d".repeat(64)),
        engine_image_digest: format!("sha256:{}", "e".repeat(64)),
        runner_image_digest: format!("sha256:{}", "f".repeat(64)),
        created_at: 1.0,
        creation_profile: Some(profile()),
    }
}
fn observed(i: &ActEngineIntent) -> ActEngineObservation {
    ActEngineObservation {
        name: i.engine_name(),
        engine_id: "1".repeat(64),
        image_digest: i.engine_image_digest.clone(),
        labels: i.required_labels(OWNER).unwrap(),
    }
}
#[test]
fn intent_is_durable_immutable_and_rollback_safe() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let mut r = Registry::create_writer(&path, OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
    }
    assert!(r.act_engine(RUN).unwrap().is_none());
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
    }
    drop(r);
    let mut r = Registry::open_writer(&path).unwrap();
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Pending
    );
    let mut changed = intent();
    changed.candidate_sha = "0".repeat(40);
    assert!(
        r.begin_immediate()
            .unwrap()
            .begin_act_engine(&changed)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .begin_act_engine(&intent())
            .is_err()
    );
}
#[test]
fn registration_rejects_foreign_identity_and_registers_ownership_atomically() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
    }
    for bad in [
        ActEngineObservation {
            labels: BTreeMap::new(),
            ..observed(&intent())
        },
        ActEngineObservation {
            image_digest: format!("sha256:{}", "0".repeat(64)),
            ..observed(&intent())
        },
        ActEngineObservation {
            name: "foreign".into(),
            ..observed(&intent())
        },
    ] {
        assert!(
            r.begin_immediate()
                .unwrap()
                .register_act_engine(RUN, &bad, 2.0)
                .is_err()
        );
    }
    assert!(r.resources(0, 10).unwrap().items.is_empty());
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(r.resources(0, 10).unwrap().items.len(), 1);
    assert_eq!(r.resource_uses(0, 10).unwrap().items.len(), 1);
}
#[test]
fn success_requires_execution_and_exact_cleanup_proof() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .request_act_cleanup(RUN, ActRunOutcome::Passed, 2.0)
            .is_err()
    );
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .request_act_cleanup(RUN, ActRunOutcome::Passed, 3.0)
            .is_err()
    );
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
            .unwrap();
        tx.record_act_execution(RUN, CLAIM, ActRunOutcome::Passed, 3.0)
            .unwrap();
        tx.request_act_execution_cleanup(RUN, CLAIM, ActRunOutcome::Passed, 4.0)
            .unwrap();
        tx.commit().unwrap();
    }
    let bad = ActEngineRemovalProof {
        name: intent().engine_name(),
        engine_id: Some("2".repeat(64)),
    };
    assert!(
        r.begin_immediate()
            .unwrap()
            .finalize_act_cleanup(RUN, &bad, 5.0)
            .is_err()
    );
    assert_eq!(r.pending_act_engines(None, 10).unwrap().items.len(), 1);
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.finalize_act_cleanup(
            RUN,
            &ActEngineRemovalProof {
                name: intent().engine_name(),
                engine_id: Some("1".repeat(64)),
            },
            5.0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let record = r.act_engine(RUN).unwrap().unwrap();
    assert_eq!(record.state, ActEngineState::Terminal);
    assert_eq!(record.outcome, Some(ActRunOutcome::Passed));
    assert!(r.pending_act_engines(None, 10).unwrap().items.is_empty());
    assert!(
        r.begin_immediate()
            .unwrap()
            .record_act_execution(RUN, CLAIM, ActRunOutcome::Failed, 6.0)
            .is_err()
    );
}
#[test]
fn pending_create_failure_is_recoverable_and_history_does_not_cap_recovery() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    for n in 0..1100 {
        let mut i = intent();
        i.run_id = format!("{n:08x}-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&i).unwrap();
        tx.request_act_cleanup(&i.run_id, ActRunOutcome::Interrupted, 2.0)
            .unwrap();
        tx.finalize_act_cleanup(
            &i.run_id,
            &ActEngineRemovalProof {
                name: i.engine_name(),
                engine_id: None,
            },
            3.0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        r.pending_act_engines(None, 1).unwrap().items[0]
            .intent
            .run_id,
        RUN
    );
}

#[test]
fn registration_and_terminal_retirement_rollback_together() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
    }
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
    }
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Pending
    );
    assert!(r.resources(0, 1).unwrap().items.is_empty());
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.request_act_cleanup(RUN, ActRunOutcome::Cancelled, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.finalize_act_cleanup(
            RUN,
            &ActEngineRemovalProof {
                name: intent().engine_name(),
                engine_id: Some("1".repeat(64)),
            },
            4.0,
        )
        .unwrap();
    }
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::CleanupRequired
    );
    assert_eq!(
        r.resources(0, 1).unwrap().items[0].state,
        bosn_core::ResourceState::Active
    );
}

#[test]
fn recovery_pages_are_explicit_and_pending_ownership_check_is_exact() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    for n in 0..3 {
        let mut i = intent();
        i.run_id = format!("{n:08x}-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&i).unwrap();
        tx.commit().unwrap();
    }
    let first = r.pending_act_engines(None, 2).unwrap();
    assert_eq!(first.items.len(), 2);
    assert_eq!(
        first.next_run_id.as_deref(),
        Some("00000001-bbbb-4ccc-8ddd-eeeeeeeeeeee")
    );
    let last = r
        .pending_act_engines(first.next_run_id.as_deref(), 2)
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next_run_id, None);
    assert!(r.pending_act_engines(None, 0).is_err());
    assert!(r.pending_act_engines(None, 1001).is_err());
    assert!(r.pending_act_engines(Some("not-a-uuid"), 2).is_err());
    assert!(
        r.pending_act_engines(Some("AAAAAAAA-BBBB-4CCC-8DDD-EEEEEEEEEEEE"), 2)
            .is_err()
    );
    let i = last.items[0].intent.clone();
    let mut foreign = observed(&i);
    foreign
        .labels
        .insert("com.zackees.bosn.registry".into(), "foreign".into());
    assert!(
        r.begin_immediate()
            .unwrap()
            .verify_act_engine(&i.run_id, &foreign)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .verify_act_engine(&i.run_id, &observed(&i))
            .is_ok()
    );
}

#[test]
fn liveness_and_conflicting_execution_protect_cleanup() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
            .unwrap();
        tx.record_act_execution(RUN, CLAIM, ActRunOutcome::Failed, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .record_act_execution(RUN, CLAIM, ActRunOutcome::Passed, 4.0)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .request_act_cleanup(RUN, ActRunOutcome::Passed, 4.0)
            .is_err()
    );
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.request_act_execution_cleanup(RUN, CLAIM, ActRunOutcome::Failed, 4.0)
            .unwrap();
        tx.put_lease(&bosn_registry::Lease {
            id: "protect".into(),
            resource_id: format!("act-engine:{RUN}"),
            pid: 1,
            proc_start: None,
            acquired_at: 4.0,
            heartbeat_at: 4.0,
            ttl_seconds: 60.0,
        })
        .unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .finalize_act_cleanup(
                RUN,
                &ActEngineRemovalProof {
                    name: intent().engine_name(),
                    engine_id: Some("1".repeat(64))
                },
                5.0
            )
            .is_err()
    );
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::CleanupRequired
    );
}

#[test]
fn existing_resource_cannot_be_overwritten_by_act_registration() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    let mut tx = r.begin_immediate().unwrap();
    tx.begin_act_engine(&intent()).unwrap();
    tx.put_resource(&bosn_registry::Resource {
        id: format!("act-engine:{RUN}"),
        kind: bosn_core::ResourceKind::Container,
        name: intent().engine_name(),
        stack: "foreign".into(),
        generation: "foreign".into(),
        scope: bosn_core::Scope::Machine,
        workspace: "/foreign".into(),
        created_at: 1.0,
        last_used: 1.0,
        state: bosn_core::ResourceState::Active,
        retention: bosn_core::Retention::Pinned,
    })
    .unwrap();
    tx.commit().unwrap();
    assert!(
        r.begin_immediate()
            .unwrap()
            .register_act_engine(RUN, &observed(&intent()), 2.0)
            .is_err()
    );
    assert_eq!(r.resources(0, 1).unwrap().items[0].stack, "foreign");
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Pending
    );
}

#[test]
fn crash_before_registration_recovers_exact_id_for_cleanup_only() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("r");
    let mut r = Registry::create_writer(&path, OWNER).unwrap();
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.request_act_cleanup(RUN, ActRunOutcome::Interrupted, 2.0)
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.recover_act_engine(RUN, &observed(&intent()), 3.0)
            .unwrap();
    }
    assert!(r.act_engine(RUN).unwrap().unwrap().engine_id.is_none());
    assert!(r.resources(0, 1).unwrap().items.is_empty());
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.recover_act_engine(RUN, &observed(&intent()), 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    drop(r);
    let mut r = Registry::open_writer(&path).unwrap();
    let record = r.act_engine(RUN).unwrap().unwrap();
    assert_eq!(record.state, ActEngineState::CleanupRequired);
    assert_eq!(record.outcome, Some(ActRunOutcome::Interrupted));
    assert_eq!(record.engine_id, Some("1".repeat(64)));
    assert!(
        r.begin_immediate()
            .unwrap()
            .record_act_execution(RUN, CLAIM, ActRunOutcome::Passed, 4.0)
            .is_err()
    );
    let different = ActEngineObservation {
        engine_id: "2".repeat(64),
        ..observed(&intent())
    };
    assert!(
        r.begin_immediate()
            .unwrap()
            .recover_act_engine(RUN, &different, 4.0)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .recover_act_engine(RUN, &observed(&intent()), 4.0)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .authorize_act_cleanup(RUN, &different)
            .is_err()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .authorize_act_cleanup(RUN, &observed(&intent()))
            .is_ok()
    );
    assert!(
        r.begin_immediate()
            .unwrap()
            .finalize_act_cleanup(
                RUN,
                &ActEngineRemovalProof {
                    name: intent().engine_name(),
                    engine_id: None
                },
                4.0
            )
            .is_err()
    );
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.finalize_act_cleanup(
            RUN,
            &ActEngineRemovalProof {
                name: intent().engine_name(),
                engine_id: Some("1".repeat(64)),
            },
            4.0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        r.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Terminal
    );
}

#[test]
fn retirement_between_recovery_pages_does_not_skip_remaining_engines() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    for n in 0..3 {
        let mut i = intent();
        i.run_id = format!("{n:08x}-bbbb-4ccc-8ddd-eeeeeeeeeeee");
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&i).unwrap();
        tx.commit().unwrap();
    }
    let first = r.pending_act_engines(None, 2).unwrap();
    for record in &first.items {
        let mut tx = r.begin_immediate().unwrap();
        tx.request_act_cleanup(&record.intent.run_id, ActRunOutcome::Interrupted, 2.0)
            .unwrap();
        tx.finalize_act_cleanup(
            &record.intent.run_id,
            &ActEngineRemovalProof {
                name: record.intent.engine_name(),
                engine_id: None,
            },
            3.0,
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let second = r
        .pending_act_engines(first.next_run_id.as_deref(), 2)
        .unwrap();
    assert_eq!(
        second.items.len(),
        1,
        "cleanup of an earlier page must not shift the cursor past unprocessed engines"
    );
    assert_eq!(
        second.items[0].intent.run_id,
        "00000002-bbbb-4ccc-8ddd-eeeeeeeeeeee"
    );
}
/// Wall-clock creation times must survive the JSON state snapshot exactly:
/// the ownership labels embed `created_at`, so a one-ULP drift on reload
/// made a correctly labelled engine fail registration (found by bosn ci's
/// coalescing test).
#[test]
fn wall_clock_created_at_round_trips_into_registration() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    for n in 0..300u32 {
        let run = format!("aaaaaaaa-bbbb-4ccc-8ddd-{n:012x}");
        let intent = ActEngineIntent {
            run_id: run.clone(),
            created_at: 1_790_903_176.0 + f64::from(n) * 0.001_234_567_891,
            ..intent()
        };
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&intent).unwrap();
        let observed = ActEngineObservation {
            engine_id: format!("{:064x}", n + 1),
            ..observed(&intent)
        };
        tx.register_act_engine(&run, &observed, intent.created_at + 1.0)
            .unwrap_or_else(|e| panic!("created_at {} drifted: {e:?}", intent.created_at));
        tx.commit().unwrap();
    }
}

#[test]
fn fractional_timestamp_and_ownership_labels_survive_durable_reopen_exactly() {
    // This value lost one ULP with approximate serde_json parsing. The change
    // altered immutable intent equality and Docker ownership labels on reopen.
    let mut requested = intent();
    requested.created_at = f64::from_bits(4_745_298_354_865_438_729);
    let original = observed(&requested);
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("registry.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    transaction.begin_act_engine(&requested).unwrap();
    transaction.commit().unwrap();
    drop(registry);
    let mut reopened = Registry::open_writer(&path).unwrap();
    let persisted = reopened.act_engine(RUN).unwrap().unwrap();
    assert_eq!(
        persisted.intent.created_at.to_bits(),
        requested.created_at.to_bits()
    );
    assert_eq!(
        persisted.intent.required_labels(OWNER).unwrap(),
        original.labels
    );
    let mut transaction = reopened.begin_immediate().unwrap();
    transaction
        .register_act_engine(RUN, &original, requested.created_at + 1.0)
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        reopened.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Registered
    );
}

#[test]
fn exclusive_claim_is_atomic_durable_and_owner_only() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("claim.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.commit().unwrap();
    }
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
            .unwrap();
    }
    assert!(
        registry
            .act_engine(RUN)
            .unwrap()
            .unwrap()
            .execution_claim
            .is_none()
    );
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    assert_eq!(
        registry
            .act_engine(RUN)
            .unwrap()
            .unwrap()
            .execution_claim
            .as_deref(),
        Some(CLAIM)
    );
    for token in [CLAIM, "98765432-1234-4234-8234-123456789abc", "INVALID"] {
        assert!(
            registry
                .begin_immediate()
                .unwrap()
                .claim_act_execution(&intent(), &observed(&intent()), token, 4.0)
                .is_err()
        );
    }
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .request_act_cleanup(RUN, ActRunOutcome::Interrupted, 4.0)
            .is_err()
    );
    let foreign = "98765432-1234-4234-8234-123456789abc";
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .record_act_execution(RUN, foreign, ActRunOutcome::Passed, 4.0)
            .is_err()
    );
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .request_act_execution_cleanup(RUN, foreign, ActRunOutcome::Interrupted, 4.0)
            .is_err()
    );
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.verify_act_execution(RUN, &observed(&intent()), CLAIM)
            .unwrap();
        tx.record_act_execution(RUN, CLAIM, ActRunOutcome::Failed, 4.0)
            .unwrap();
        tx.request_act_execution_cleanup(RUN, CLAIM, ActRunOutcome::Failed, 5.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        registry
            .act_engine(RUN)
            .unwrap()
            .unwrap()
            .execution_claim
            .as_deref(),
        Some(CLAIM)
    );
}

#[test]
fn legacy_registered_snapshot_is_recoverable_but_cannot_execute() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(dir.path().join("legacy"), OWNER).unwrap();
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.register_act_engine(RUN, &observed(&intent()), 2.0)
            .unwrap();
        tx.commit().unwrap();
    }
    let mut old = serde_json::to_value(registry.act_engine(RUN).unwrap().unwrap()).unwrap();
    old["schema_version"] = serde_json::json!(1);
    old["intent"]
        .as_object_mut()
        .unwrap()
        .remove("creation_profile");
    old.as_object_mut().unwrap().remove("execution_claim");
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.append_event(
            2.0,
            &format!("act.engine.v1:{RUN}"),
            &serde_json::to_string(&old).unwrap(),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        registry.pending_act_engines(None, 16).unwrap().items.len(),
        1
    );
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
            .is_err()
    );
    {
        let mut tx = registry.begin_immediate().unwrap();
        tx.request_act_cleanup(RUN, ActRunOutcome::Interrupted, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        registry.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::CleanupRequired
    );
}

#[test]
fn frozen_profile_is_validated_bound_to_labels_and_reopened_exactly() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("profile.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut absent = intent();
    absent.creation_profile = None;
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .begin_act_engine(&absent)
            .is_err()
    );
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_act_engine(&intent()).unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let record = registry.act_engine(RUN).unwrap().unwrap();
    assert_eq!(record.schema_version, 3);
    assert_eq!(record.intent.creation_profile, Some(profile()));
    assert_eq!(
        profile().digest().unwrap(),
        "72d8760f51a8a89bacf590bc6a7c90b43f4eeb543f396f376025feeddc1f6938"
    );
    let labels = intent().required_labels(OWNER).unwrap();
    assert_eq!(
        labels["com.zackees.bosn.act.creation-profile-sha256"],
        profile().digest().unwrap()
    );
    assert_eq!(
        labels["com.zackees.bosn.act.init-command-sha256"],
        "a".repeat(64)
    );
    for field in [
        "memory_bytes",
        "storage_bytes",
        "nano_cpus",
        "pids",
        "run_tmpfs_bytes",
        "tmp_tmpfs_bytes",
        "init_command_sha256",
    ] {
        let mut v = serde_json::to_value(profile()).unwrap();
        v[field] = if field == "init_command_sha256" {
            serde_json::json!("A".repeat(64))
        } else {
            serde_json::json!(0)
        };
        let invalid: ActEngineCreationProfile = serde_json::from_value(v).unwrap();
        assert!(invalid.validate().is_err(), "{field}");
    }
    for field in [
        "memory_bytes",
        "storage_bytes",
        "nano_cpus",
        "pids",
        "run_tmpfs_bytes",
        "tmp_tmpfs_bytes",
        "init_command_sha256",
    ] {
        let mut value = serde_json::to_value(profile()).unwrap();
        value[field] = if field == "init_command_sha256" {
            serde_json::json!("b".repeat(64))
        } else {
            serde_json::json!(
                value[field].as_u64().unwrap() + if field == "nano_cpus" { 10000 } else { 1 }
            )
        };
        let changed_profile: ActEngineCreationProfile = serde_json::from_value(value).unwrap();
        changed_profile.validate().unwrap();
        assert_ne!(
            changed_profile.digest().unwrap(),
            profile().digest().unwrap(),
            "{field}"
        );
        let mut changed = intent();
        changed.creation_profile = Some(changed_profile);
        assert!(
            registry
                .begin_immediate()
                .unwrap()
                .register_act_engine(RUN, &observed(&changed), 2.0)
                .is_err(),
            "{field}"
        );
    }
    assert_eq!(
        registry.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::Pending
    );
    for (key, bad) in [
        ("tmpfs_policy", serde_json::json!("storage_all_exec")),
        ("untrusted_argv", serde_json::json!("dockerd")),
    ] {
        let mut malformed = serde_json::to_value(profile()).unwrap();
        malformed[key] = bad;
        assert!(serde_json::from_value::<ActEngineCreationProfile>(malformed).is_err());
    }
}
#[test]
fn legacy_v2_claim_cannot_resume_or_complete_but_owner_can_request_cleanup() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("legacy-v2.sqlite3");
    let mut registry = Registry::create_writer(&path, OWNER).unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.begin_act_engine(&intent()).unwrap();
    tx.register_act_engine(RUN, &observed(&intent()), 2.0)
        .unwrap();
    tx.claim_act_execution(&intent(), &observed(&intent()), CLAIM, 3.0)
        .unwrap();
    tx.commit().unwrap();
    let mut old = serde_json::to_value(registry.act_engine(RUN).unwrap().unwrap()).unwrap();
    old["schema_version"] = serde_json::json!(2);
    old["intent"]
        .as_object_mut()
        .unwrap()
        .remove("creation_profile");
    let mut tx = registry.begin_immediate().unwrap();
    tx.append_event(
        3.0,
        &format!("act.engine.v1:{RUN}"),
        &serde_json::to_string(&old).unwrap(),
    )
    .unwrap();
    tx.commit().unwrap();
    drop(registry);
    let mut registry = Registry::open_writer(&path).unwrap();
    let old = registry.act_engine(RUN).unwrap().unwrap();
    assert_eq!(old.intent.creation_profile, None);
    let old_observed = observed(&old.intent);
    let mut unclaimed = serde_json::to_value(&old).unwrap();
    unclaimed["execution_claim"] = serde_json::json!(null);
    let mut tx = registry.begin_immediate().unwrap();
    tx.append_event(
        3.0,
        &format!("act.engine.v1:{RUN}"),
        &serde_json::to_string(&unclaimed).unwrap(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .claim_act_execution(&old.intent, &old_observed, CLAIM, 4.0)
            .is_err()
    );
    let mut tx = registry.begin_immediate().unwrap();
    tx.append_event(
        3.0,
        &format!("act.engine.v1:{RUN}"),
        &serde_json::to_string(&old).unwrap(),
    )
    .unwrap();
    tx.commit().unwrap();

    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .verify_act_execution(RUN, &old_observed, CLAIM)
            .is_err()
    );
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .record_act_execution(RUN, CLAIM, ActRunOutcome::Passed, 4.0)
            .is_err()
    );
    assert!(
        registry
            .begin_immediate()
            .unwrap()
            .request_act_cleanup(RUN, ActRunOutcome::Interrupted, 4.0)
            .is_err()
    );
    let mut tx = registry.begin_immediate().unwrap();
    tx.request_act_execution_cleanup(RUN, CLAIM, ActRunOutcome::Interrupted, 4.0)
        .unwrap();
    tx.commit().unwrap();
    assert_eq!(
        registry.act_engine(RUN).unwrap().unwrap().state,
        ActEngineState::CleanupRequired
    );
}

#[test]
fn record_versions_refuse_profile_presence_mismatches() {
    for (version, has_profile) in [(1, true), (2, true), (3, false), (4, true)] {
        let dir = TemporaryDirectory::new().unwrap();
        let mut registry = Registry::create_writer(dir.path().join("bad-version"), OWNER).unwrap();
        let mut tx = registry.begin_immediate().unwrap();
        tx.begin_act_engine(&intent()).unwrap();
        tx.commit().unwrap();
        let mut value = serde_json::to_value(registry.act_engine(RUN).unwrap().unwrap()).unwrap();
        value["schema_version"] = serde_json::json!(version);
        if !has_profile {
            value["intent"]
                .as_object_mut()
                .unwrap()
                .remove("creation_profile");
        }
        let mut tx = registry.begin_immediate().unwrap();
        tx.append_event(
            1.0,
            &format!("act.engine.v1:{RUN}"),
            &serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(
            registry.act_engine(RUN).is_err(),
            "version{version},profile{has_profile}"
        );
    }
}
