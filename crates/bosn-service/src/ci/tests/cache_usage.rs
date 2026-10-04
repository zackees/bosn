//! Diagnostic read failures remain visible in the public runner reply.

use super::*;

#[test]
fn failed_cache_measurement_is_unknown_and_partial_in_the_runner_reply() {
    with_registry(|registry, dir| async move {
        let backend = Arc::new(FakeBackend::with(Faults {
            cache_measure: true,
            ..Default::default()
        }));
        let runtime = CiRuntime::start(&dir, registry, backend, 1);
        let reply: RunnersReply = call(
            &runtime,
            CiRequest::Runners {
                action: RunnerAction::CacheUsage,
            },
        )
        .await;
        let cache = reply.cache.unwrap();
        assert!(cache.partial);
        assert_eq!(cache.bytes, None);
        assert!(cache.errors[0].contains("Docker unavailable"));
    });
}
