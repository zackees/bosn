//! A run samples its engine's storage while act runs and says when it ran low (#392).

use super::*;
use crate::ci::storage::StorageUsage;

const GIB: u64 = 1 << 30;

fn run_with_storage(usage: Option<StorageUsage>, exit_code: i32) -> (EngineReport, Vec<String>) {
    let mut out = None;
    let slot = &mut out;
    with_registry(|registry, _dir| async move {
        let backend = FakeBackend::with(Faults {
            exit_code,
            ..Faults::default()
        });
        *backend.storage.lock().unwrap() = usage;
        let mut seen = Collect::default();
        let report = run_on_engine(
            &registry,
            &backend,
            &plan(&run_id(1), Duration::from_secs(5)),
            &CancellationSource::new().token(),
            &mut seen,
        )
        .await;
        *slot = Some((report, seen.notes));
    });
    out.unwrap()
}

#[test]
fn a_run_on_nearly_full_storage_records_the_peak_and_warns_in_the_log() {
    let low = StorageUsage {
        size: 20 * GIB,
        used: 16 * GIB,
        available: 4 * GIB,
    };
    let (report, notes) = run_with_storage(Some(low), 1);
    assert_eq!(report.execution, ExecutionEnd::Exited(1));
    assert_eq!(report.storage, Some(low));
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("warning: the engine storage backing filesystem is low")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n == "engine storage backing filesystem peaked at 16.0 of 20.0 GiB used, 4.0 GiB free (filesystem-wide; not engine-owned bytes)"),
        "{notes:?}"
    );
}

#[test]
fn an_unsampled_engine_still_runs_and_reports_no_peak() {
    let (report, notes) = run_with_storage(None, 0);
    assert_eq!(report.execution, ExecutionEnd::Exited(0));
    assert_eq!(report.cleanup, CleanupEnd::Removed);
    assert_eq!(report.storage, None);
    assert!(
        notes
            .iter()
            .any(|n| n.starts_with("engine storage not sampled")),
        "{notes:?}"
    );
}

/// Drain `count` lines sent as fast as possible, then the sender's EOF after
/// `close_after`, while each storage sample takes `delay` (#538).
fn drain_with_slow_probe(
    delay: Duration,
    count: usize,
    close_after: Duration,
) -> (
    Collect,
    StoragePeak,
    Option<String>,
    (u32, u32, u32),
    Duration,
) {
    let mut out = None;
    let slot = &mut out;
    with_registry(|_registry, _dir| async move {
        let backend = FakeBackend::default();
        *backend.storage.lock().unwrap() = Some(StorageUsage {
            size: 20 * GIB,
            used: 16 * GIB,
            available: 4 * GIB,
        });
        *backend.storage_delay.lock().unwrap() = delay;
        let (lines, mut receiver) = async_engine::channel(4);
        let send = async move {
            for n in 0..count {
                lines
                    .send(EngineLine::Stdout(format!("line {n}")))
                    .await
                    .unwrap();
            }
            async_engine::sleep(close_after).await;
        };
        let (mut peak, mut unsampled, mut seen) =
            (StoragePeak::default(), None, Collect::default());
        let started = Instant::now();
        let drain = drain_output(
            &backend,
            "engine",
            &mut receiver,
            &mut peak,
            &mut unsampled,
            &mut seen,
        );
        let bounded =
            async_engine::timeout(Duration::from_secs(15), async_engine::join(send, drain));
        assert!(
            bounded.await.is_ok(),
            "the drain stalled behind the storage probe"
        );
        let elapsed = started.elapsed();
        let probes = *backend.storage_probes.lock().unwrap();
        *slot = Some((seen, peak, unsampled, probes, elapsed));
    });
    out.unwrap()
}

#[test]
fn a_slow_storage_probe_never_blocks_draining_output() {
    let (seen, peak, _, probes, elapsed) =
        drain_with_slow_probe(Duration::from_secs(60), 500, Duration::ZERO);
    assert_eq!(seen.lines.len(), 500);
    // EOF waited only the short grace for the 60 s probe, never the probe itself.
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");
    // One probe started and never overlapped.
    assert_eq!(probes.0, 1, "{probes:?}");
    assert_eq!(probes.2, 1, "{probes:?}");
    assert_eq!(peak.peak(), None);
}

#[test]
fn a_probe_that_finishes_while_draining_keeps_the_peak_and_does_not_overlap() {
    let (seen, peak, unsampled, probes, _) =
        drain_with_slow_probe(Duration::from_millis(50), 200, Duration::from_millis(400));
    assert_eq!(seen.lines.len(), 200);
    assert_eq!(unsampled, None);
    assert_eq!(peak.peak().map(|usage| usage.used), Some(16 * GIB));
    assert!(
        seen.notes
            .iter()
            .any(|n| n.starts_with("warning: the engine storage")),
        "{:?}",
        seen.notes
    );
    // The next sample waits SAMPLE_INTERVAL after the last finished: no busy loop.
    assert_eq!(probes.2, 1, "{probes:?}");
    assert!(probes.0 <= 2, "{probes:?}");
}
