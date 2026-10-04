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
