//! Startup recovery: interrupt and retire every engine a previous daemon left, then seal.

use super::*;

/// Startup-only limits. The daemon must call this before accepting requests.
#[derive(Clone, Copy, Debug)]
pub struct ActStartupRecoveryOptions {
    pub page_size: usize,
    pub max_runs: usize,
    pub deadline: Duration,
}
#[derive(Debug, Default)]
pub struct ActStartupRecoveryReport {
    pub runs: Vec<ActStartupRecoveryResult>,
}
#[derive(Debug)]
pub struct ActStartupRecoveryResult {
    pub run_id: String,
    pub engine_removed: bool,
    /// A refusal leaves durable cleanup pending; it is not execution evidence.
    pub deferred_reason: Option<String>,
}
pub(super) struct StartupSealGuard(Option<RegistryActor>);
impl Drop for StartupSealGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.0.take() {
            async_engine::launch(async move {
                // Lost callers cannot keep recovery authority open. Admission
                // must still depend on the explicit successful seal below.
                let _ = async_engine::timeout(
                    Duration::from_secs(5),
                    registry.act_registry(ActRegistryCommand::SealStartup),
                )
                .await;
            })
            .detach();
        }
    }
}

/// Interrupt and retire stale owned engines; never resume an old execution.
/// Image proofs must come from server-owned verified publisher bytes.
/// A successful return includes a durable seal; errors must prevent admission.
pub async fn recover_startup_act_engines(
    registry: &RegistryActor,
    engine: &DockerEngine,
    owner: &str,
    proofs: &BTreeMap<String, crate::act_engine::VerifiedEngineManifest>,
    options: ActStartupRecoveryOptions,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<ActStartupRecoveryReport> {
    let mut seal = StartupSealGuard(Some(registry.clone()));
    let work = async {
        if options.page_size == 0
            || options.page_size > 1000
            || options.max_runs == 0
            || options.max_runs > 10000
            || options.deadline <= Duration::from_secs(5)
        {
            return Err(error("invalid bounded startup recovery options"));
        }
        let started = Instant::now();
        // Engine work gets four fifths of the guarded window. The rest is headroom for marking
        // and deferring every remaining record once the budget is spent, so an exhausted budget
        // defers work instead of reaching the outer guard, which would stop the daemon (#555).
        let budget = recovery_window(options.deadline) / 5 * 4;
        let mut report = ActStartupRecoveryReport::default();
        let mut cursor = None;
        loop {
            if cancellation.is_cancelled() {
                return Err(error("startup recovery cancelled"));
            }
            let reply = registry
                .act_registry(ActRegistryCommand::Pending {
                    after_run_id: cursor.clone(),
                    limit: options.page_size,
                })
                .await
                .map_err(|e| error(e.to_string()))?;
            let ActRegistryReply::Recovery(page) = reply else {
                return Err(error("startup recovery page reply mismatch"));
            };
            if report.runs.len().saturating_add(page.items.len()) > options.max_runs {
                return Err(error("startup recovery active-run ceiling exceeded"));
            }
            for record in page.items {
                registry
                    .act_registry(ActRegistryCommand::StartupInterrupt {
                        run: record.intent.run_id.clone(),
                        at: now().max(record.updated_at),
                    })
                    .await
                    .map_err(|e| error(e.to_string()))?;
                let remaining = budget.saturating_sub(started.elapsed());
                let result = retire_engine(
                    registry,
                    engine,
                    owner,
                    proofs,
                    &record,
                    remaining,
                    cancellation,
                )
                .await;
                report.runs.push(ActStartupRecoveryResult {
                    run_id: record.intent.run_id,
                    engine_removed: result.is_ok(),
                    deferred_reason: result.err().map(|e| e.to_string()),
                });
            }
            match page.next_run_id {
                Some(next) if cursor.as_ref().is_none_or(|old| old < &next) => cursor = Some(next),
                Some(_) => return Err(error("startup recovery cursor did not advance")),
                None => return Ok(report),
            }
        }
    };
    let result = async_engine::timeout(recovery_window(options.deadline), work)
        .await
        .map_err(|_| error("startup recovery deadline exceeded"))
        .and_then(|r| r);
    let sealed = async_engine::timeout(
        Duration::from_secs(5),
        registry.act_registry(ActRegistryCommand::SealStartup),
    )
    .await
    .map_err(|_| error("startup recovery seal deadline exceeded"))?
    .map_err(|e| error(format!("startup recovery seal failed: {e}")))?;
    if !matches!(sealed, ActRegistryReply::Committed) {
        return Err(error("startup recovery seal reply mismatch"));
    }
    seal.0 = None;
    result
}
/// The part of the deadline recovery may use; the last five seconds are the seal's.
fn recovery_window(deadline: Duration) -> Duration {
    deadline.saturating_sub(Duration::from_secs(5))
}

pub(super) async fn recovery_control(
    engine: &DockerEngine,
    args: Vec<String>,
    deadline: Instant,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<Vec<u8>> {
    if cancellation.is_cancelled() {
        return Err(error("startup recovery cancelled"));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(error("startup recovery deadline exceeded"));
    }
    let output = engine
        .with_args(args)
        .capture_async(RunOptions::bounded(
            remaining.min(Duration::from_secs(10)),
            2 << 20,
        ))
        .await
        .map_err(|e| error(e.to_string()))?;
    if output.exit_code != 0 {
        return Err(error(format!(
            "startup engine probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}
/// Retire one `cleanup_required` engine: prove its name and ID absent (then
/// finalize), or verify the observed engine is exactly the committed one
/// (recording an unregistered engine's identity for cleanup only) and remove
/// it through registry authorization. Startup recovery and `bosn ci`'s
/// in-run cleanup share this path; a failure leaves the record pending.
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) async fn retire_engine(
    registry: &RegistryActor,
    engine: &DockerEngine,
    owner: &str,
    proofs: &BTreeMap<String, crate::act_engine::VerifiedEngineManifest>,
    record: &bosn_registry::act::ActEngineRecord,
    remaining: Duration,
    cancellation: &async_engine::CancellationToken,
) -> std::io::Result<()> {
    use crate::act_engine::{
        frozen_limits, observe_engine, observe_engine_image_from_manifest, remove_owned_engine,
    };
    let intent = &record.intent;
    if record.registry_id != owner {
        return Err(error("startup recovery registry owner mismatch"));
    }
    let limits = frozen_limits(intent).map_err(|e| error(e.to_string()))?;
    let proof = proofs
        .get(&intent.engine_image_digest)
        .ok_or_else(|| error("startup recovery missing trusted engine image proof"))?;
    let deadline = Instant::now()
        .checked_add(remaining)
        .ok_or_else(|| error("invalid recovery deadline"))?;
    // A successful list cannot settle an unobserved create request: Docker
    // may finish that request after its CLI client was killed or timed out.
    // Keep that intent quarantined until an immutable ID can be reconciled.
    let name = intent.engine_name();
    let mut named = Vec::new();
    for filter in std::iter::once(format!("name=^/{name}$"))
        .chain(record.engine_id.as_ref().map(|id| format!("id={id}")))
    {
        let bytes = recovery_control(
            engine,
            vec![
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--no-trunc".into(),
                "--filter".into(),
                filter,
                "--format".into(),
                "{{.ID}}".into(),
            ],
            deadline,
            cancellation,
        )
        .await?;
        let ids = std::str::from_utf8(&bytes)
            .map_err(|_| error("invalid engine list encoding"))?
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if ids.len() > 1
            || ids.iter().any(|id| {
                id.len() != 64
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(error("ambiguous startup engine identity"));
        }
        named.push(ids);
    }
    if named.iter().all(Vec::is_empty) {
        confirm_absence(record)?;
        // Engine absence is the writer-quiescence half of the source stop; the
        // retained-volume ownership probe is the other half. Both must hold
        // before a native recovery record can advance.
        crate::act_engine::stop_source_writers(registry, engine, record, owner)
            .await
            .map_err(|e| error(e.to_string()))?;
        crate::act_engine::remove_storage_volume(engine, record, owner)
            .await
            .map_err(|e| error(e.to_string()))?;
        registry
            .act_registry(ActRegistryCommand::Finalize {
                run: intent.run_id.clone(),
                proof: bosn_registry::act::ActEngineRemovalProof {
                    storage_volume: intent.storage_volume_name(),
                    name,
                    engine_id: record.engine_id.clone(),
                },
                at: now().max(record.updated_at),
            })
            .await
            .map_err(|e| error(e.to_string()))?;
        return Ok(());
    }
    let id = named[0]
        .first()
        .ok_or_else(|| error("engine ID exists with unexpected name"))?;
    if record
        .engine_id
        .as_ref()
        .is_some_and(|expected| expected != id)
        || named.get(1).is_some_and(|ids| ids.first() != Some(id))
    {
        return Err(error("startup engine name/ID mismatch"));
    }
    let image = recovery_control(
        engine,
        vec![
            "image".into(),
            "inspect".into(),
            format!("docker.io/library/docker@{}", intent.engine_image_digest),
        ],
        deadline,
        cancellation,
    )
    .await?;
    let image = observe_engine_image_from_manifest(&image, intent, proof)
        .map_err(|e| error(e.to_string()))?;
    let bytes = recovery_control(
        engine,
        vec!["container".into(), "inspect".into(), id.clone()],
        deadline,
        cancellation,
    )
    .await?;
    let observed =
        observe_engine(&bytes, intent, owner, &image, limits).map_err(|e| error(e.to_string()))?;
    if record.engine_id.is_none() {
        registry
            .act_registry(ActRegistryCommand::Recover {
                run: intent.run_id.clone(),
                observed: observed.clone(),
                at: now().max(record.updated_at),
            })
            .await
            .map_err(|e| error(e.to_string()))?;
    }
    // Refuse deletion unless all commands and persistence fit the remaining budget.
    let reserved = crate::act_engine::removal_reserve(intent.storage_volume_name().is_some());
    if cancellation.is_cancelled() || deadline.saturating_duration_since(Instant::now()) < reserved
    {
        return Err(error(
            "startup removal deferred: insufficient reserved cleanup budget",
        ));
    }
    remove_owned_engine(
        registry,
        engine,
        &intent.run_id,
        observed,
        now().max(record.updated_at),
    )
    .await
    .map_err(|e| error(e.to_string()))
}

/// An empty lookup proves removal only after this creation was observed, or when `docker
/// create` was provably never sent (#554). No waiting period establishes that an unresolved
/// Docker request is done.
pub(crate) fn confirm_absence(record: &bosn_registry::act::ActEngineRecord) -> std::io::Result<()> {
    if record.engine_id.is_none() && record.create_requested != Some(false) {
        return Err(error(
            "engine creation remains unresolved; retaining cleanup_required for reconciliation",
        ));
    }
    Ok(())
}
