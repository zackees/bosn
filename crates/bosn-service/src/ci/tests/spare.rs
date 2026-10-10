//! The runtime keeps one spare engine (#410): prepared while idle on a host
//! with room, claimed by the next run, replaced, opted out by config, and
//! removed when the daemon stops.

use super::*;
use crate::ci::{reply::SpareState, spare::ROOM};

async fn spare(runtime: &CiRuntime) -> Option<SpareStatus> {
    call::<RunnersReply>(
        runtime,
        CiRequest::Runners {
            action: RunnerAction::List,
        },
    )
    .await
    .runners
    .spare
}

/// Until the runtime reports a ready spare other than `not`.
async fn ready(runtime: &CiRuntime, not: Option<&str>) -> SpareStatus {
    for _ in 0..500 {
        if let Some(status) = spare(runtime).await
            && status.state == SpareState::Ready
            && Some(status.engine.as_str()) != not
        {
            return status;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("no ready spare");
}

async fn until_live(backend: &FakeBackend, live: usize) {
    for _ in 0..500 {
        if backend.live() == live {
            return;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("{} engines live, expected {live}", backend.live());
}

fn engine_id(backend: &FakeBackend, name: &str) -> String {
    backend.engines.lock().unwrap()[name].engine_id.clone()
}

/// Run one job to success: this daemon now runs CI and keeps a spare.
async fn first_run(runtime: &CiRuntime, sha_byte: char) {
    let run = submit(runtime, sha_byte).await.run;
    let done = wait_done(runtime, &run).await;
    assert_eq!(
        done.conclusion,
        Some(Conclusion::Success),
        "{:?}",
        done.reason
    );
}

#[test]
fn a_daemon_that_never_ran_ci_keeps_no_spare() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::roomy());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        spare(&runtime).await;
        async_engine::sleep(Duration::from_millis(100)).await;
        assert_eq!(backend.live(), 0);
        assert!(spare(&runtime).await.is_none());
    });
}

#[test]
fn a_run_claims_the_spare_and_the_next_one_is_prepared() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::roomy());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        first_run(&runtime, '0').await;
        let first = ready(&runtime, None).await;
        let preparations = *backend.engine_preparations.lock().unwrap();
        let first_id = engine_id(&backend, &first.engine);
        let run = submit(&runtime, 'a').await.run;
        let done = wait_done(&runtime, &run).await;
        assert_eq!(
            done.conclusion,
            Some(Conclusion::Success),
            "{:?}",
            done.reason
        );
        assert_eq!(
            done.engine_id.as_deref(),
            Some(first_id.as_str()),
            "ran on the spare"
        );
        assert_eq!(done.cleanup.as_deref(), Some("removed"));
        let second = ready(&runtime, Some(&first.engine)).await;
        assert_ne!(second.engine, first.engine);
        // Cap one: more kicks never make a second spare.
        for _ in 0..5 {
            spare(&runtime).await;
        }
        async_engine::sleep(Duration::from_millis(100)).await;
        assert_eq!(backend.live(), 1);
        // Only the replacement was prepared; the run used the spare as it was.
        assert_eq!(
            *backend.engine_preparations.lock().unwrap(),
            preparations + 1
        );
        runtime.close_spares().await;
        assert_eq!(backend.live(), 0, "nothing is left after the daemon stops");
        assert!(spare(&runtime).await.is_none());
    });
}

#[test]
fn a_host_without_room_keeps_no_spare() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::default());
        const { assert!(super::super::lifecycle::tests::FAKE_HOST.available_memory < ROOM) };
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        first_run(&runtime, '1').await;
        spare(&runtime).await;
        async_engine::sleep(Duration::from_millis(100)).await;
        assert_eq!(backend.live(), 0);
        assert!(spare(&runtime).await.is_none());
    });
}

#[test]
fn spares_zero_opts_out_and_retires_a_kept_spare() {
    with_registry(|registry, dir| async move {
        let config = dir.join("config.toml");
        std::fs::write(&config, "[engine]\nshared = false\nspares = 0\n").unwrap();
        let backend = Arc::new(FakeBackend::roomy());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        spare(&runtime).await;
        async_engine::sleep(Duration::from_millis(100)).await;
        assert_eq!(backend.live(), 0, "opted out: no spare");
        let run = submit(&runtime, 'b').await.run;
        assert_eq!(
            wait_done(&runtime, &run).await.conclusion,
            Some(Conclusion::Success)
        );
        assert_eq!(
            *backend.engine_preparations.lock().unwrap(),
            1,
            "the run's own"
        );
        std::fs::write(&config, "[engine]\nshared = false\nspares = 1\n").unwrap();
        ready(&runtime, None).await;
        std::fs::write(&config, "[engine]\nshared = false\nspares = 0\n").unwrap();
        spare(&runtime).await;
        until_live(&backend, 0).await;
        assert!(spare(&runtime).await.is_none());
    });
}

#[test]
fn clearing_the_cache_retires_the_spare_first() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::roomy());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        first_run(&runtime, '2').await;
        ready(&runtime, None).await;
        let cleared: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::ClearCache,
            },
        )
        .await;
        assert_eq!(cleared.cache.unwrap().bytes, None);
        // A new spare is prepared once the volume is gone.
        ready(&runtime, None).await;
        assert_eq!(backend.live(), 1);
        runtime.close_spares().await;
        assert_eq!(backend.live(), 0);
    });
}

#[test]
fn online_retry_updates_the_run_bound_to_a_claimed_spare() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::roomy());
        let runtime = CiRuntime::start(&dir, registry, backend.clone(), 2);
        first_run(&runtime, '0').await;
        let held = ready(&runtime, None).await;
        let held_id = engine_id(&backend, &held.engine);
        backend.faults.lock().unwrap().remove = true;
        let run = submit(&runtime, 'a').await.run;
        let done = wait_done(&runtime, &run).await;
        assert_eq!(done.engine_id.as_deref(), Some(held_id.as_str()));
        assert!(done.cleanup.as_deref().unwrap().starts_with("failed:"));
        backend.faults.lock().unwrap().remove = false;
        let retry = runtime
            .retry_cleanup(crate::ci::lifecycle::tests::OWNER, None)
            .await
            .unwrap();
        assert!(retry.retired.is_some());
        let updated = runtime.record(&run).unwrap();
        assert_eq!(
            updated.cleanup.as_deref(),
            Some("removed"),
            "the bound run, not the spare UUID, needs the updated receipt"
        );
        assert_eq!(updated.conclusion, done.conclusion);
        runtime.close_spares().await;
    });
}
