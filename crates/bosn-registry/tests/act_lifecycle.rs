use bosn_registry::{Registry, act::*};
use kernal_api::platform::fs::TemporaryDirectory;
use std::collections::BTreeMap;
const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const RUN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
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
        tx.record_act_execution(RUN, ActRunOutcome::Passed, 3.0)
            .unwrap();
        tx.request_act_cleanup(RUN, ActRunOutcome::Passed, 4.0)
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
            .record_act_execution(RUN, ActRunOutcome::Failed, 6.0)
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
        tx.record_act_execution(RUN, ActRunOutcome::Failed, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .record_act_execution(RUN, ActRunOutcome::Passed, 4.0)
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
        tx.request_act_cleanup(RUN, ActRunOutcome::Failed, 4.0)
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
            .record_act_execution(RUN, ActRunOutcome::Passed, 4.0)
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
