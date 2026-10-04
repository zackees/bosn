use super::*;
use crate::ci::lifecycle::tests::{OWNER, record_of};
use bosn_registry::act::ActEngineState;

#[test]
fn online_retry_recovers_failed_cleanup_without_restarting_or_changing_verdict() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            remove: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry.clone(), backend.clone(), 1);
        let run = submit(&runtime, 'a').await.run;
        let done = wait_done(&runtime, &run).await;
        assert_eq!(
            record_of(&registry, &run).await.state,
            ActEngineState::CleanupRequired
        );
        let failed = runtime.retry_cleanup(OWNER, None).await.unwrap();
        assert!(failed.retired.is_none());
        assert!(failed.deferred.is_some());
        assert_eq!(backend.live(), 1);
        backend.faults.lock().unwrap().remove = false;
        let recovered = runtime.retry_cleanup(OWNER, None).await.unwrap();
        assert_eq!(recovered.retired.as_deref(), Some(run.as_str()));
        assert_eq!(backend.live(), 0);
        assert_eq!(
            record_of(&registry, &run).await.state,
            ActEngineState::Terminal
        );
        let record = runtime.record(&run).unwrap();
        assert_eq!(record.cleanup.as_deref(), Some("removed"));
        assert_eq!(
            record.conclusion, done.conclusion,
            "cleanup retry cannot rewrite execution evidence"
        );
        assert!(
            runtime
                .retry_cleanup(OWNER, None)
                .await
                .unwrap()
                .retired
                .is_none()
        );
    });
}

#[test]
fn online_retry_preserves_active_engine_and_execution_claim() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            hang: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry.clone(), backend.clone(), 1);
        let run = submit(&runtime, 'a').await.run;
        for _ in 0..200 {
            if *backend.executions.lock().unwrap() > 0 {
                break;
            }
            async_engine::sleep(Duration::from_millis(10)).await;
        }
        let before = record_of(&registry, &run).await;
        assert!(before.execution_claim.is_some());
        let pass = runtime.retry_cleanup(OWNER, None).await.unwrap();
        assert!(pass.retired.is_none());
        assert_eq!(backend.live(), 1);
        assert_eq!(
            record_of(&registry, &run).await.execution_claim,
            before.execution_claim
        );
        let cancelled: CancelReply = call(&runtime, CiRequest::Cancel { run: run.clone() }).await;
        assert!(cancelled.cancelled);
        wait_done(&runtime, &run).await;
    });
}

#[test]
fn online_retry_cursor_does_not_let_a_failed_engine_starve_later_cleanup() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            remove: true,
            ..Faults::default()
        }));
        let runtime = CiRuntime::start(&dir, registry.clone(), backend.clone(), 1);
        let a = submit(&runtime, 'a').await.run;
        wait_done(&runtime, &a).await;
        let b = submit(&runtime, 'b').await.run;
        wait_done(&runtime, &b).await;
        let failed = runtime.retry_cleanup(OWNER, None).await.unwrap();
        assert!(failed.retired.is_none());
        let first = failed.next_cursor.clone().unwrap();
        backend.faults.lock().unwrap().remove = false;
        let next = runtime
            .retry_cleanup(OWNER, failed.next_cursor)
            .await
            .unwrap();
        assert_ne!(next.retired.as_deref(), Some(first.as_str()));
        assert!(next.retired.is_some());
        assert_eq!(
            record_of(&registry, &first).await.state,
            ActEngineState::CleanupRequired
        );
        let wrap = runtime
            .retry_cleanup(OWNER, next.next_cursor)
            .await
            .unwrap();
        assert!(wrap.next_cursor.is_none());
        let retried = runtime
            .retry_cleanup(OWNER, wrap.next_cursor)
            .await
            .unwrap();
        assert_eq!(retried.retired.as_deref(), Some(first.as_str()));
        assert_eq!(backend.live(), 0);
    });
}
