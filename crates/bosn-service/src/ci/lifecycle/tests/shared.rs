//! The shared engine (#547): concurrent runs lease slots in one engine,
//! each closes its own scope, and the engine is retired only when idle.
use super::*;
use crate::ci::{
    shared_engine::{Refused, SharedEngine},
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

#[test]
fn concurrent_leases_share_one_engine_on_distinct_slots() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults::default()));
        let shared = SharedEngine::new(registry.clone(), backend.clone());
        let cancel = CancellationSource::new().token();
        let a = shared.lease(want(1, &dir, 8), &cancel).await.unwrap();
        let b = shared.lease(want(2, &dir, 8), &cancel).await.unwrap();
        assert_eq!(a.engine_id, b.engine_id, "one engine");
        assert_ne!(a.slot, b.slot, "distinct slots, so distinct ports");
        assert_eq!(backend.live(), 1);
        // A run that wants a different engine is refused while this one is busy.
        assert_eq!(
            shared.lease(want(3, &dir, 16), &cancel).await.unwrap_err(),
            Refused::Mismatched
        );
        shared.release(a).await;
        assert!(!shared.retire_idle(Duration::ZERO).await, "still busy");
        shared.release(b).await;
        assert!(
            !shared.retire_idle(Duration::from_secs(600)).await,
            "not idle long enough"
        );
        assert_eq!(backend.live(), 1);
        assert!(shared.retire_idle(Duration::ZERO).await);
        assert_eq!(backend.live(), 0, "an idle engine is retired and removed");
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
                &registry,
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
