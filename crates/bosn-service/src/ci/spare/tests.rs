//! The spare engine's lifecycle (#410) over the synthetic engine host: claim
//! (once, under a race), retirement on a profile change, startup
//! reconciliation, the cap of one, and a failed preparation.

use super::*;
use crate::act_registry::{ActRegistryCommand, ActRegistryReply};
use crate::ci::{
    engine::act_artifact,
    lifecycle::{
        CleanupEnd, ExecutionEnd, run_on_engine,
        tests::{
            Collect, FakeBackend, Faults, intent, plan, record_of, restarted, run_id, terminal,
            test_cache, with_daemon, with_registry,
        },
    },
};
use bosn_registry::act::{ActEngineBinding, ActEngineState};
use kernal_api::async_engine::CancellationSource;

/// A spare that would be exactly the engine `intent(run_id(n))` describes.
fn spare_plan(n: u32) -> SparePlan {
    let unbound = ActEngineBinding::spare(&run_id(n), "/state");
    SparePlan {
        intent: ActEngineIntent {
            run_id: unbound.run_id,
            workspace: unbound.workspace,
            candidate_sha: unbound.candidate_sha,
            payload_sha256: unbound.payload_sha256,
            snapshot_sha256: unbound.snapshot_sha256,
            spare: true,
            ..intent(&run_id(n))
        },
        act: act_artifact("x86_64").unwrap(),
        cache: test_cache(),
    }
}

async fn filled(keeper: &SpareKeeper, n: u32) {
    let fill = keeper.begin().expect("no spare yet");
    keeper.fill(fill, Ok(Some(spare_plan(n)))).await;
}

fn keeper(registry: &RegistryActor, backend: &Arc<FakeBackend>) -> Arc<SpareKeeper> {
    let backend: Arc<dyn ActEngineBackend> = backend.clone();
    let keeper = Arc::new(SpareKeeper::new(registry.clone(), backend));
    keeper.want();
    keeper
}

fn far() -> async_engine::Deadline {
    async_engine::Deadline::after(Duration::from_secs(5))
}

async fn absent(registry: &RegistryActor, run: &str) -> bool {
    matches!(
        registry
            .act_registry(ActRegistryCommand::Get { run: run.into() })
            .await
            .unwrap(),
        ActRegistryReply::Record(None)
    )
}

#[test]
fn a_run_claims_the_prepared_spare_and_never_prepares_an_engine() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let keeper = keeper(&registry, &backend);
        filled(&keeper, 900).await;
        let status = keeper.status().unwrap();
        assert_eq!(status.state, SpareState::Ready);
        assert_eq!(status.engine, format!("bosn-act-{}", run_id(900)));
        assert_eq!(
            (backend.live(), *backend.engine_preparations.lock().unwrap()),
            (1, 1)
        );
        let held = record_of(&registry, &run_id(900)).await;
        assert_eq!(held.state, ActEngineState::Registered);
        assert!(
            held.execution_claim.is_some(),
            "prepared under the daemon's claim"
        );

        let mut run = plan(&run_id(1), Duration::from_secs(5));
        run.spare = keeper
            .take(&run.intent, far(), &CancellationSource::new().token())
            .await;
        assert!(run.spare.is_some() && keeper.status().is_none());
        let mut seen = Collect::default();
        let report = run_on_engine(
            &registry,
            backend.as_ref(),
            &run,
            &CancellationSource::new().token(),
            &mut seen,
        )
        .await;
        assert_eq!(report.execution, ExecutionEnd::Exited(0));
        assert_eq!(report.cleanup, CleanupEnd::Removed);
        assert_eq!(
            report.engine_id.as_deref(),
            Some(held.engine_id.as_deref().unwrap())
        );
        assert_eq!(
            *backend.engine_preparations.lock().unwrap(),
            1,
            "no second prepare"
        );
        let notes = seen.notes.join("\n");
        assert!(notes.contains(&format!(
            "claiming prepared spare engine bosn-act-{}",
            run_id(900)
        )));
        assert!(notes.contains("spare engine claimed in "), "{notes}");
        assert!(!notes.contains("creating isolated engine"), "{notes}");
        assert!(!notes.contains("preparing engine"), "{notes}");
        // Spare -> claimed by the run -> removed, under the spare's own key.
        let done = record_of(&registry, &run_id(900)).await;
        terminal(&done, ActRunOutcome::Passed);
        assert_eq!(done.binding, Some(run.intent.binding()));
        assert!(
            absent(&registry, &run_id(1)).await,
            "no engine of the run's own"
        );
        assert_eq!(backend.live(), 0);
    });
}

#[test]
fn two_runs_racing_for_one_spare_claim_it_once() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let keeper = keeper(&registry, &backend);
        filled(&keeper, 901).await;
        let never = CancellationSource::new();
        let (a, b) = (
            plan(&run_id(2), Duration::from_secs(5)),
            plan(&run_id(3), Duration::from_secs(5)),
        );
        let (first, second) = async_engine::join(
            keeper.take(&a.intent, far(), &never.token()),
            keeper.take(&b.intent, far(), &never.token()),
        )
        .await;
        assert!(
            first.is_some() != second.is_some(),
            "the keeper hands it out once"
        );
        // Even handed to both, the registry lets exactly one claim it.
        let spare = first.or(second);
        let (mut a, mut b) = (a, b);
        a.spare = spare.clone();
        b.spare = spare;
        let (mut seen_a, mut seen_b) = (Collect::default(), Collect::default());
        let (ra, rb) = async_engine::join(
            run_on_engine(&registry, backend.as_ref(), &a, &never.token(), &mut seen_a),
            run_on_engine(&registry, backend.as_ref(), &b, &never.token(), &mut seen_b),
        )
        .await;
        for report in [&ra, &rb] {
            assert_eq!(report.execution, ExecutionEnd::Exited(0));
            assert_eq!(report.cleanup, CleanupEnd::Removed);
        }
        let claimed = [&seen_a, &seen_b]
            .iter()
            .filter(|seen| {
                seen.notes
                    .iter()
                    .any(|n| n.starts_with("spare engine claimed in"))
            })
            .count();
        assert_eq!(claimed, 1);
        let spare = record_of(&registry, &run_id(901)).await;
        terminal(&spare, ActRunOutcome::Passed);
        let winner = spare.binding.unwrap().run_id;
        assert!(winner == run_id(2) || winner == run_id(3));
        assert_eq!(backend.live(), 0, "the loser's own engine is gone too");
        assert_eq!(*backend.executions.lock().unwrap(), 2);
    });
}

#[test]
fn a_spare_made_for_other_limits_or_pins_is_retired_not_claimed() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let keeper = keeper(&registry, &backend);
        let mut limits = intent(&run_id(4));
        limits.creation_profile.as_mut().unwrap().memory_bytes += 1 << 30;
        let mut pins = intent(&run_id(5));
        pins.runner_image_digest = format!("sha256:{}", "0".repeat(64));
        for (n, want) in [(902, limits), (903, pins)] {
            filled(&keeper, n).await;
            let token = CancellationSource::new().token();
            assert!(keeper.take(&want, far(), &token).await.is_none());
            keeper.retire_all().await;
            terminal(
                &record_of(&registry, &run_id(n)).await,
                ActRunOutcome::Cancelled,
            );
            assert!(record_of(&registry, &run_id(n)).await.binding.is_none());
            assert_eq!(backend.live(), 0);
        }
    });
}

#[test]
fn startup_recovery_retires_a_held_spare_and_a_half_created_one() {
    with_daemon(|daemon, path| async move {
        let backend = Arc::new(FakeBackend::default());
        filled(&keeper(&daemon.registry, &backend), 904).await;
        // A spare whose engine exists but was never registered: the daemon
        // died between Docker's create and the registry.
        let half = spare_plan(905).intent;
        daemon
            .registry
            .act_registry(ActRegistryCommand::Begin(half.clone()))
            .await
            .unwrap();
        backend.insert(
            &half.engine_name(),
            &half.engine_image_digest,
            half.required_labels(crate::ci::lifecycle::tests::OWNER)
                .unwrap(),
        );
        assert_eq!(backend.live(), 2);
        daemon.stop().await;
        let (daemon, mut retired, failed) = restarted(&path, &backend).await;
        assert!(failed.is_empty(), "{failed:?}");
        retired.sort();
        assert_eq!(retired, [run_id(904), run_id(905)]);
        assert_eq!(backend.live(), 0);
        for n in [904, 905] {
            terminal(
                &record_of(&daemon.registry, &run_id(n)).await,
                ActRunOutcome::Interrupted,
            );
        }
        daemon.stop().await;
    });
}

#[test]
fn at_most_one_spare_is_kept() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let keeper = keeper(&registry, &backend);
        let fill = keeper.begin().unwrap();
        assert!(keeper.begin().is_none(), "one is being prepared");
        keeper.fill(fill, Ok(Some(spare_plan(906)))).await;
        assert!(keeper.begin().is_none(), "one is kept");
        assert_eq!(backend.live(), 1);
        keeper.close().await;
        assert!(keeper.begin().is_none(), "closed");
        assert_eq!(backend.live(), 0);
        terminal(
            &record_of(&registry, &run_id(906)).await,
            ActRunOutcome::Cancelled,
        );
    });
}

#[test]
fn a_run_waits_for_the_spare_being_prepared() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::default());
        let keeper = keeper(&registry, &backend);
        let fill = keeper.begin().unwrap();
        let want = intent(&run_id(6));
        let never = CancellationSource::new();
        let (taken, ()) = async_engine::join(keeper.take(&want, far(), &never.token()), async {
            async_engine::sleep(Duration::from_millis(50)).await;
            keeper.fill(fill, Ok(Some(spare_plan(907)))).await;
        })
        .await;
        let spare = taken.expect("the run got the spare it waited for");
        assert_eq!(spare.intent.run_id, run_id(907));
        retire(&registry, backend.as_ref(), &spare).await.unwrap();
        assert_eq!(backend.live(), 0);
    });
}

#[test]
fn a_spare_that_cannot_be_prepared_leaves_nothing_and_waits_to_retry() {
    with_registry(|registry, _dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            prepare: true,
            ..Faults::default()
        }));
        let keeper = keeper(&registry, &backend);
        filled(&keeper, 908).await;
        assert!(keeper.status().is_none());
        assert_eq!(backend.live(), 0);
        terminal(
            &record_of(&registry, &run_id(908)).await,
            ActRunOutcome::Cancelled,
        );
        assert!(keeper.begin().is_none(), "no retry within a minute");
    });
}

#[test]
fn spares_config_is_zero_or_one() {
    let parse = |text: &str| toml::from_str::<crate::ci::limits::EngineConfig>(text);
    assert_eq!(parse("").unwrap().spares, Spares::One);
    assert_eq!(parse("spares = 0").unwrap().spares, Spares::None);
    assert_eq!(parse("spares = 1").unwrap().spares, Spares::One);
    assert!(parse("spares = 2").is_err());
}
