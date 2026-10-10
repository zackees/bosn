//! Spare engines (#410) through the registry: a spare is prepared under its
//! holder's claim, handed to exactly one run atomically, and recovered like
//! any other owned engine.
use bosn_registry::{Registry, act::*};
use kernal_api::platform::fs::TemporaryDirectory;

const OWNER: &str = "11111111-2222-4333-8444-555555555555";
const HOLDER: &str = "12345678-1234-4234-8234-123456789abc";
const RUNNER: &str = "87654321-4321-4321-8321-cba987654321";
const SPARE: &str = "aaaaaaaa-bbbb-4ccc-8ddd-000000000001";
const RUN: &str = "aaaaaaaa-bbbb-4ccc-8ddd-0000000000f1";

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
        cache_volume: None,
        cache_coordination: None,
        tool_generation: None,
        docker_socket: None,
    }
}

fn spare(run: &str) -> ActEngineIntent {
    let unbound = ActEngineBinding::spare(run, "/state");
    ActEngineIntent {
        run_id: unbound.run_id,
        workspace: unbound.workspace,
        candidate_sha: unbound.candidate_sha,
        payload_sha256: unbound.payload_sha256,
        snapshot_sha256: unbound.snapshot_sha256,
        act_version: "0.2.88".into(),
        act_image_digest: format!("sha256:{}", "d".repeat(64)),
        engine_image_digest: format!("sha256:{}", "e".repeat(64)),
        runner_image_digest: format!("sha256:{}", "f".repeat(64)),
        created_at: 1.0,
        creation_profile: Some(profile()),
        spare: true,
    }
}

fn binding() -> ActEngineBinding {
    ActEngineBinding {
        run_id: RUN.into(),
        workspace: "/private/source".into(),
        candidate_sha: "a".repeat(40),
        payload_sha256: "b".repeat(64),
        snapshot_sha256: "c".repeat(64),
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

/// A registered spare held by `HOLDER`, as the daemon leaves it prepared.
fn held_spare(r: &mut Registry) {
    let intent = spare(SPARE);
    let mut tx = r.begin_immediate().unwrap();
    tx.begin_act_engine(&intent).unwrap();
    tx.register_act_engine(SPARE, &observed(&intent), 2.0)
        .unwrap();
    tx.claim_act_execution(&intent, &observed(&intent), HOLDER, 3.0)
        .unwrap();
    tx.commit().unwrap();
}

fn claim(
    r: &mut Registry,
    from: &str,
    token: &str,
) -> Result<ActEngineRecord, bosn_registry::Error> {
    let intent = spare(SPARE);
    let mut tx = r.begin_immediate().unwrap();
    let claimed = tx.claim_act_spare(SPARE, &observed(&intent), from, token, &binding(), 4.0)?;
    tx.commit().unwrap();
    Ok(claimed)
}

#[test]
fn a_spare_names_no_run_and_says_so_in_its_labels() {
    let labels = spare(SPARE).required_labels(OWNER).unwrap();
    assert_eq!(labels["com.zackees.bosn.act.spare"], "true");
    assert_eq!(labels["com.zackees.bosn.act.candidate-sha"], "0".repeat(40));
    // A run's intent is unchanged: no spare label.
    let mut run = spare(SPARE);
    run.spare = false;
    run.candidate_sha = "a".repeat(40);
    assert!(
        !run.required_labels(OWNER)
            .unwrap()
            .contains_key("com.zackees.bosn.act.spare")
    );
    // A spare cannot carry a run's source.
    let mut bound = spare(SPARE);
    bound.candidate_sha = "a".repeat(40);
    assert!(bound.required_labels(OWNER).is_err());
    // Nor exist without a frozen profile.
    let mut legacy = spare(SPARE);
    legacy.creation_profile = None;
    assert!(legacy.required_labels(OWNER).is_err());
}

#[test]
fn exactly_one_run_claims_a_spare_and_the_holder_loses_it() {
    let dir = TemporaryDirectory::new().unwrap();
    let path = dir.path().join("r");
    let mut r = Registry::create_writer(&path, OWNER).unwrap();
    held_spare(&mut r);
    // Only the holder's claim can hand the spare over.
    assert!(claim(&mut r, RUNNER, "99999999-1234-4234-8234-123456789abc").is_err());
    let claimed = claim(&mut r, HOLDER, RUNNER).unwrap();
    assert_eq!(claimed.execution_claim.as_deref(), Some(RUNNER));
    assert_eq!(claimed.binding, Some(binding()));
    // The race's loser: neither the old nor the new claim hands it over again.
    assert!(claim(&mut r, HOLDER, "99999999-1234-4234-8234-123456789abc").is_err());
    assert!(claim(&mut r, RUNNER, HOLDER).is_err());
    // The holder can no longer retire it; the run owns its cleanup.
    assert!(
        r.begin_immediate()
            .unwrap()
            .request_act_execution_cleanup(SPARE, HOLDER, ActRunOutcome::Cancelled, 5.0)
            .is_err()
    );
    drop(r);
    let mut r = Registry::open_writer(&path).unwrap();
    assert_eq!(
        r.act_engine(SPARE).unwrap().unwrap().binding,
        Some(binding())
    );
    let mut tx = r.begin_immediate().unwrap();
    tx.record_act_execution(SPARE, RUNNER, ActRunOutcome::Passed, 6.0)
        .unwrap();
    tx.request_act_execution_cleanup(SPARE, RUNNER, ActRunOutcome::Passed, 7.0)
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn only_an_unused_held_spare_can_be_claimed() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    // A run's own engine is never a spare.
    let mut run = spare(RUN);
    run.spare = false;
    run.candidate_sha = "a".repeat(40);
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&run).unwrap();
        tx.register_act_engine(RUN, &observed(&run), 2.0).unwrap();
        tx.claim_act_execution(&run, &observed(&run), HOLDER, 3.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert!(
        r.begin_immediate()
            .unwrap()
            .claim_act_spare(RUN, &observed(&run), HOLDER, RUNNER, &binding(), 4.0)
            .is_err()
    );
    // A spare whose holder already asked for its removal.
    held_spare(&mut r);
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.request_act_execution_cleanup(SPARE, HOLDER, ActRunOutcome::Cancelled, 4.0)
            .unwrap();
        tx.commit().unwrap();
    }
    assert!(claim(&mut r, HOLDER, RUNNER).is_err());
}

#[test]
fn startup_recovery_lists_held_and_half_created_spares() {
    let dir = TemporaryDirectory::new().unwrap();
    let mut r = Registry::create_writer(dir.path().join("r"), OWNER).unwrap();
    held_spare(&mut r);
    // A spare whose engine was never registered (the daemon died mid-create).
    let half = "aaaaaaaa-bbbb-4ccc-8ddd-000000000002";
    {
        let mut tx = r.begin_immediate().unwrap();
        tx.begin_act_engine(&spare(half)).unwrap();
        tx.commit().unwrap();
    }
    let page = r.pending_act_engines(None, 10).unwrap();
    let listed: Vec<_> = page
        .items
        .iter()
        .map(|r| r.intent.run_id.as_str())
        .collect();
    assert_eq!(listed, [SPARE, half]);
    // Startup interruption takes both to cleanup, the held one through its
    // holder's claim, exactly as for a run's engine.
    let mut tx = r.begin_immediate().unwrap();
    tx.request_act_execution_cleanup(SPARE, HOLDER, ActRunOutcome::Interrupted, 5.0)
        .unwrap();
    tx.request_act_cleanup(half, ActRunOutcome::Interrupted, 5.0)
        .unwrap();
    tx.commit().unwrap();
}

#[test]
fn a_spare_matches_only_an_identical_engine() {
    let base = spare(SPARE);
    let mut run = base.clone();
    run.spare = false;
    run.run_id = RUN.into();
    run.candidate_sha = "a".repeat(40);
    assert!(base.same_engine(&run), "run-bound fields do not matter");
    let mut limits = run.clone();
    limits.creation_profile.as_mut().unwrap().memory_bytes += 1 << 30;
    assert!(
        !base.same_engine(&limits),
        "a host-sized limit changed (#379/#392)"
    );
    let mut pins = run.clone();
    pins.runner_image_digest = format!("sha256:{}", "0".repeat(64));
    assert!(!base.same_engine(&pins), "a pin changed (#378)");
}

#[test]
fn a_spare_matches_whatever_its_socket_directory_is_named() {
    let socket = |dir: &str, group: u32| ActEngineDockerSocket {
        host_dir: dir.into(),
        group,
    };
    let mut base = spare(SPARE);
    base.creation_profile.as_mut().unwrap().docker_socket = Some(socket("/s/aaaaaaaa", 100));
    let mut run = base.clone();
    run.run_id = RUN.into();
    run.creation_profile.as_mut().unwrap().docker_socket = Some(socket("/s/bbbbbbbb", 100));
    assert!(
        base.same_engine(&run),
        "each engine names its own directory"
    );
    let mut group = run.clone();
    group.creation_profile.as_mut().unwrap().docker_socket = Some(socket("/s/bbbbbbbb", 0));
    assert!(
        !base.same_engine(&group),
        "the socket's group is the engine's"
    );
    let mut none = run;
    none.creation_profile.as_mut().unwrap().docker_socket = None;
    assert!(
        !base.same_engine(&none),
        "an engine without the socket differs"
    );
}
