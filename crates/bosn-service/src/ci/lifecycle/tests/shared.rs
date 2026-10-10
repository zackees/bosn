//! The shared engine (#547, #544): concurrent runs lease slots in one
//! engine per machine, from any daemon, each closes its own scope, and the
//! engine is retired only when no daemon's run holds it.
use super::*;
use crate::ci::{
    machine::{Daemon as Process, EngineClaim},
    shared_engine::{Lease, SharedEngine},
    spare::SparePlan,
};

fn want(n: u32, dir: &std::path::Path, memory_gib: u64) -> SparePlan {
    let mut intent = intent(&run_id(0x5000 + n));
    let unbound = ActEngineBinding::spare(&intent.run_id, "/state");
    intent.workspace = unbound.workspace;
    intent.candidate_sha = unbound.candidate_sha;
    intent.payload_sha256 = unbound.payload_sha256;
    intent.snapshot_sha256 = unbound.snapshot_sha256;
    intent.spare = true;
    let mut profile = intent.creation_profile.take().unwrap();
    profile.memory_bytes = memory_gib << 30;
    let socket = bosn_registry::act::ActEngineDockerSocket {
        host_dir: dir.join(format!("e{n}")).to_string_lossy().into_owned(),
        group: 0,
    };
    intent.creation_profile = Some(crate::act_engine::with_docker_socket(profile, socket).unwrap());
    SparePlan {
        intent,
        act: super::super::super::engine::act_artifact("x86_64").unwrap(),
        cache: test_cache(),
    }
}

/// A plan for an engine with other pins: no run of it may use this one.
fn other_pins(n: u32, dir: &std::path::Path) -> SparePlan {
    let mut plan = want(n, dir, 8);
    plan.intent.act_version = "0.0.1".into();
    plan
}

/// A second daemon: its own registry, with its own owner.
fn second_daemon(dir: &std::path::Path) -> Daemon {
    let path = dir.join("second.sqlite3");
    let registry = Registry::create_writer(&path, "99999999-2222-4333-8444-555555555555").unwrap();
    let (sender, receiver) = async_engine::channel(16);
    let task = async_engine::launch(registry_actor(registry, receiver, None));
    Daemon {
        registry: RegistryActor { sender },
        task,
    }
}

async fn lease(shared: &SharedEngine, n: u32, plan: SparePlan) -> Result<Lease, String> {
    let cancel = CancellationSource::new().token();
    shared
        .lease(&run_id(n), plan, &cancel, &mut |_: &str| {})
        .await
}

/// A lease that must still be waiting after a while, with what it said.
async fn waits(shared: &SharedEngine, n: u32, plan: SparePlan) -> Vec<String> {
    let cancel = CancellationSource::new().token();
    let mut notes = Vec::new();
    let leased = async_engine::timeout(
        Duration::from_millis(300),
        shared.lease(&run_id(n), plan, &cancel, &mut |why: &str| {
            notes.push(why.to_owned())
        }),
    )
    .await;
    assert!(leased.is_err(), "the lease should wait");
    notes
}

#[test]
fn concurrent_leases_share_one_engine_on_distinct_slots() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        let a = lease(&shared, 1, want(1, &dir, 8)).await.unwrap();
        // Sizing differs, the pins do not: the same engine serves it.
        let b = lease(&shared, 2, want(2, &dir, 16)).await.unwrap();
        assert_eq!(a.engine_id, b.engine_id, "one engine");
        assert_ne!(a.slot, b.slot, "distinct slots, so distinct ports");
        assert_eq!(backend.live(), 1);
        assert!(backend.machine.claim.lock().unwrap().is_some(), "claimed");
        shared.release(a).await;
        assert!(!shared.retire_idle(Duration::ZERO).await, "still busy");
        shared.release(b).await;
        assert!(shared.retire_idle(Duration::ZERO).await);
        assert_eq!(backend.live(), 0, "an idle engine is retired and removed");
        assert!(
            backend.machine.claim.lock().unwrap().is_none(),
            "and its claim released"
        );
    });
}

#[test]
fn an_engine_with_other_pins_waits_for_the_busy_one_to_drain() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        let a = lease(&shared, 1, want(1, &dir, 8)).await.unwrap();
        let notes = waits(&shared, 2, other_pins(2, &dir)).await;
        assert!(
            notes.iter().any(|n| n.contains("waiting for its runs")),
            "{notes:?}"
        );
        assert_eq!(backend.live(), 1, "never a second engine");
        let first = a.engine_id.clone();
        shared.release(a).await;
        let b = lease(&shared, 2, other_pins(2, &dir)).await.unwrap();
        assert_eq!(backend.live(), 1, "the drained engine was replaced");
        assert_ne!(b.engine_id, first);
        shared.release(b).await;
        shared.close().await;
        assert_eq!(backend.live(), 0);
    });
}

#[test]
fn two_daemons_share_one_engine_and_only_its_maker_retires_it() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let other = second_daemon(&dir);
        let first = SharedEngine::new(registry.clone(), backend.clone());
        let second = SharedEngine::new(other.registry.clone(), backend.clone());
        let (a, b) = async_engine::join(
            lease(&first, 1, want(1, &dir, 8)),
            lease(&second, 2, want(2, &dir, 8)),
        )
        .await;
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.engine_id, b.engine_id, "one engine for both daemons");
        assert_ne!(a.slot, b.slot);
        assert_eq!(backend.live(), 1);
        assert_eq!(*backend.engine_preparations.lock().unwrap(), 1);
        let first_made = backend
            .machine
            .claim
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .registry
            == OWNER;
        let (maker, user, mine, theirs) = if first_made {
            (&first, &second, a, b)
        } else {
            (&second, &first, b, a)
        };
        maker.release(mine).await;
        assert!(
            !maker.retire_idle(Duration::ZERO).await,
            "another daemon's run still holds it"
        );
        assert!(!user.retire_idle(Duration::ZERO).await, "not its engine");
        user.release(theirs).await;
        assert!(maker.retire_idle(Duration::ZERO).await);
        assert_eq!(backend.live(), 0);
        assert!(backend.machine.claim.lock().unwrap().is_none());
        other.stop().await;
    });
}

#[test]
fn a_claim_whose_daemon_died_is_taken_over() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let me = Process::current();
        *backend.machine.claim.lock().unwrap() = Some(EngineClaim {
            id: "claim-dead".into(),
            engine: format!("bosn-act-{}", run_id(0x9999)),
            registry: "99999999-2222-4333-8444-555555555555".into(),
            daemon: Process {
                start: me.start + 1,
                ..me
            },
            created: 0,
        });
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        let a = lease(&shared, 1, want(1, &dir, 8)).await.unwrap();
        let claim = backend.machine.claim.lock().unwrap().clone().unwrap();
        assert_ne!(
            claim.id, "claim-dead",
            "the dead daemon's claim was replaced"
        );
        assert_eq!(claim.registry, OWNER);
        shared.release(a).await;
        shared.close().await;
    });
}

#[test]
fn legacy_engines_drain_before_the_shared_engine_is_made() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let mut labels = intent(&run_id(0x7001)).required_labels(OWNER).unwrap();
        labels.insert("com.zackees.bosn.created".into(), "1000".into());
        let busy = backend.insert("bosn-act-legacy-busy", "sha256:x", labels.clone());
        let idle = backend.insert("bosn-act-legacy-idle", "sha256:x", labels);
        backend
            .machine
            .busy
            .lock()
            .unwrap()
            .insert(busy.engine_id.clone());
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        // Idle is believed only when seen twice.
        let mut notes = waits(&shared, 1, want(1, &dir, 8)).await;
        assert!(backend.machine.removed.lock().unwrap().is_empty());
        notes.extend(waits(&shared, 1, want(1, &dir, 8)).await);
        assert!(
            notes.iter().any(|n| n.contains("legacy engine")),
            "{notes:?}"
        );
        assert_eq!(
            *backend.machine.removed.lock().unwrap(),
            vec![idle.name.clone()],
            "the idle one is retired, the busy one left to finish"
        );
        assert!(
            backend.machine.claim.lock().unwrap().is_none(),
            "not made yet"
        );
        backend.machine.busy.lock().unwrap().clear();
        let a = lease(&shared, 1, want(1, &dir, 8)).await.unwrap();
        assert_eq!(backend.live(), 1, "only the shared engine is left");
        shared.release(a).await;
        shared.close().await;
    });
}

#[test]
fn runs_reuse_the_engine_and_close_only_their_own_scope() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        let cancel = CancellationSource::new().token();
        let mut engines = Vec::new();
        for n in 1..=2 {
            let mut seen = Collect::default();
            let report = super::super::shared::run_on_shared(
                backend.as_ref(),
                &shared,
                want(n, &dir, 8),
                &plan(&run_id(n), Duration::from_secs(5)),
                &cancel,
                &mut seen,
            )
            .await;
            assert_eq!(
                report.execution,
                ExecutionEnd::Exited(0),
                "{:?}",
                seen.notes
            );
            assert_eq!(report.cleanup, CleanupEnd::Removed);
            assert!(
                seen.notes
                    .iter()
                    .any(|n| n.starts_with("run scope cleaned up")),
                "{:?}",
                seen.notes
            );
            engines.push(report.engine_id.unwrap());
            assert_eq!(backend.live(), 1, "the engine outlives the run");
        }
        assert_eq!(engines[0], engines[1], "the second run reused the engine");
        assert_eq!(
            *backend.engine_preparations.lock().unwrap(),
            1,
            "prepared once"
        );
        assert_eq!(backend.closed_scopes.lock().unwrap().len(), 2);
        shared.close().await;
        assert_eq!(backend.live(), 0, "shutdown retires the idle engine");
    });
}

#[test]
fn an_idle_engine_whose_maker_died_is_reclaimed_and_retired_by_another_daemon() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let other = second_daemon(&dir);
        let maker = SharedEngine::new(other.registry.clone(), backend.clone());
        let user = SharedEngine::new(registry.clone(), backend.clone());
        let made = lease(&maker, 1, want(1, &dir, 8)).await.unwrap();
        let used = lease(&user, 2, want(2, &dir, 8)).await.unwrap();
        // The maker's daemon dies: its claim and its run's slot name a dead process.
        let me = Process::current();
        let dead = Process {
            start: me.start + 1,
            ..me
        };
        {
            let mut claim = backend.machine.claim.lock().unwrap();
            claim.as_mut().unwrap().daemon = dead.clone();
            let mut tables = backend.machine.tables.lock().unwrap();
            let table = tables.get_mut(&made.engine_id).unwrap();
            table.held.get_mut(&made.slot).unwrap().daemon = dead;
        }
        user.tend_machine(Duration::ZERO).await;
        assert_eq!(backend.live(), 1, "a live run of another daemon keeps it");
        user.release(used).await;
        user.tend_machine(Duration::ZERO).await;
        assert_eq!(backend.live(), 0, "retired once only dead holders remain");
        assert!(backend.machine.claim.lock().unwrap().is_none());
        assert!(
            backend
                .closed_scopes
                .lock()
                .unwrap()
                .iter()
                .any(|label| *label == run_id(1)),
            "the dead run's scope was closed first"
        );
        other.stop().await;
    });
}
