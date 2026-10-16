//! One act run in the shared engine (#547): lease a slot, have act2's serve
//! open the run's scope (cgroup, network, work tree, ports, Docker proxy),
//! run act, close the scope (everything with the run's label removed and
//! proven gone, then the tool cache saved), and give the slot back. The engine itself stays up for the next run; it is
//! retired only when idle ([`super::super::shared_engine`]).
//!
//! A run never gets an engine of its own here (#544): when the machine's
//! engine is not one it can use, it waits for that engine to drain and retire.

use kernal_api::async_engine::{self, CancellationToken};

use super::{
    super::{
        engine::{ActEngineBackend, ActInvocation, ExecEnd, RunLimits, RunScope},
        shared_engine::{Lease, SharedEngine},
        spare::SparePlan,
        storage::{self, StoragePeak},
    },
    CleanupEnd, EngineObserver, EnginePlan, EngineReport, ExecutionEnd, Laps, close_scope,
    drain_output,
};

/// Run `plan` in the machine's shared engine.
pub async fn run_on_shared(
    backend: &dyn ActEngineBackend,
    shared: &SharedEngine,
    want: SparePlan,
    plan: &EnginePlan,
    cancellation: &CancellationToken,
    observer: &mut dyn EngineObserver,
) -> EngineReport {
    let mut laps = Laps::new();
    observer.note("leasing a slot in the shared engine");
    let run = plan.intent.run_id.clone();
    let leased = {
        let mut note = |why: &str| observer.note(why);
        async_engine::timeout_at(
            plan.deadline,
            async_engine::cancellable(
                cancellation,
                shared.lease(&run, want, cancellation, &mut note),
            ),
        )
        .await
    };
    let lease = match leased {
        Err(_) => return report(ExecutionEnd::TimedOut, CleanupEnd::Removed, None, None),
        Ok(Err(_)) => return report(ExecutionEnd::Cancelled, CleanupEnd::Removed, None, None),
        Ok(Ok(Err(error))) => {
            return report(
                ExecutionEnd::EngineFailed(error),
                CleanupEnd::Removed,
                None,
                None,
            );
        }
        Ok(Ok(Ok(lease))) => lease,
    };
    observer.note(&format!(
        "running in shared engine {} (slot {}) after {}",
        lease.engine_name,
        lease.slot,
        laps.lap()
    ));
    let engine_id = lease.engine_id.clone();
    let mut peak = StoragePeak::default();
    let (execution, cleanup) = in_scope(
        backend,
        shared,
        &lease,
        plan,
        cancellation,
        observer,
        &mut laps,
        &mut peak,
    )
    .await;
    shared.release(lease).await;
    observer.note(&format!("slot released; total {}", laps.total()));
    report(execution, cleanup, Some(engine_id), peak.peak())
}

fn report(
    execution: ExecutionEnd,
    cleanup: CleanupEnd,
    engine_id: Option<String>,
    storage: Option<storage::StorageUsage>,
) -> EngineReport {
    EngineReport {
        execution,
        cleanup,
        engine_id,
        storage,
    }
}

/// Everything between leasing and releasing the slot. The scope is closed
/// on every path once it may exist.
#[expect(clippy::too_many_arguments, reason = "one run's whole context")]
async fn in_scope(
    backend: &dyn ActEngineBackend,
    shared: &SharedEngine,
    lease: &Lease,
    plan: &EnginePlan,
    cancellation: &CancellationToken,
    observer: &mut dyn EngineObserver,
    laps: &mut Laps,
    peak: &mut StoragePeak,
) -> (ExecutionEnd, CleanupEnd) {
    let limits = RunLimits::within_engine(lease.memory_bytes, lease.nano_cpus, lease.pids);
    let scope = match RunScope::new(&plan.intent.run_id, lease.slot, limits) {
        Ok(scope) => scope,
        Err(error) => return (ExecutionEnd::EngineFailed(error), CleanupEnd::Removed),
    };
    let mut invocation = ActInvocation {
        scope: Some(scope.clone()),
        ..plan.invocation.clone()
    };
    let engine = lease.engine_id.as_str();
    if let Some(github) = &plan.github {
        // The relay must listen before act starts any job (#650).
        match github.attach(backend, engine, &scope).await {
            Ok(port) => {
                invocation
                    .params
                    .env
                    .insert(crate::github_bootstrap::PORT_ENV.into(), port.to_string());
                observer.note(&format!(
                    "GitHub proxy relay listening in the engine on port {port}"
                ));
            }
            Err(error) => {
                return (
                    ExecutionEnd::EngineFailed(format!("github proxy relay: {error}")),
                    CleanupEnd::Removed,
                );
            }
        }
    }
    let end = execute(
        backend,
        shared,
        lease,
        plan,
        &invocation,
        cancellation,
        observer,
        laps,
        peak,
    )
    .await;
    if let Some(github) = &plan.github
        && let Err(error) = github.detach(backend, engine).await
    {
        observer.note(&format!("GitHub proxy relay not stopped cleanly: {error}"));
    }
    let cleanup = match close_scope(backend, engine, &scope, observer, laps).await {
        Ok(()) => CleanupEnd::Removed,
        Err(error) => CleanupEnd::Failed(error),
    };
    (end, cleanup)
}

#[expect(clippy::too_many_arguments, reason = "one run's whole context")]
async fn execute(
    backend: &dyn ActEngineBackend,
    shared: &SharedEngine,
    lease: &Lease,
    plan: &EnginePlan,
    invocation: &ActInvocation,
    cancellation: &CancellationToken,
    observer: &mut dyn EngineObserver,
    laps: &mut Laps,
    peak: &mut StoragePeak,
) -> ExecutionEnd {
    let engine = lease.engine_id.as_str();
    let deadline = plan.deadline;
    if let Err(error) = shared.verify(lease).await {
        return ExecutionEnd::EngineFailed(error);
    }
    observer.note("preparing run: run scope, tool cache, frozen source");
    let generation = plan
        .intent
        .creation_profile
        .as_ref()
        .and_then(|profile| profile.tool_generation.as_ref());
    let prepare = backend.prepare_run(engine, invocation, &plan.source, &plan.event, generation);
    match async_engine::timeout_at(deadline, async_engine::cancellable(cancellation, prepare)).await
    {
        Err(_) => return ExecutionEnd::TimedOut,
        Ok(Err(_)) => return ExecutionEnd::Cancelled,
        Ok(Ok(Err(error))) => return ExecutionEnd::EngineFailed(error),
        Ok(Ok(Ok(()))) => observer.note(&format!("run prepared in {}", laps.lap())),
    }
    match async_engine::timeout_at(deadline, backend.list(engine, invocation)).await {
        Err(_) => return ExecutionEnd::TimedOut,
        Ok(Ok(listing)) => observer.declared(&listing),
        Ok(Err(error)) => return ExecutionEnd::EngineFailed(error),
    }
    if let Err(error) = shared.verify(lease).await {
        return ExecutionEnd::EngineFailed(error);
    }
    observer.note("running act in its scope in the shared engine");
    let (lines, mut receiver) = async_engine::channel(512);
    let run = async {
        let end = backend
            .execute(
                engine,
                invocation,
                deadline.remaining(),
                cancellation,
                &lines,
            )
            .await;
        drop(lines);
        end
    };
    let mut unsampled = None;
    let drain = drain_output(
        backend,
        engine,
        &mut receiver,
        peak,
        &mut unsampled,
        observer,
    );
    let (end, ()) = async_engine::join(run, drain).await;
    observer.note(&format!("act finished in {}", laps.lap()));
    match (peak.peak(), unsampled) {
        (Some(usage), _) => observer.note(&storage::peak_note(usage)),
        (None, Some(error)) => observer.note(&format!("engine storage not sampled: {error}")),
        (None, None) => {}
    }
    match end {
        Ok(ExecEnd::Exited(code)) => ExecutionEnd::Exited(code),
        Ok(ExecEnd::TimedOut) => ExecutionEnd::TimedOut,
        Ok(ExecEnd::Cancelled) => ExecutionEnd::Cancelled,
        Err(error) => ExecutionEnd::EngineFailed(error),
    }
}
