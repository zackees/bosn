//! Applying token-bound GC, release, repair and stop decisions after ownership rechecks.

use super::*;

pub(crate) const SETUP_GC_ENGINE_DEADLINE: Duration = Duration::from_secs(5);
pub(crate) const SETUP_GC_ENGINE_OUTPUT: usize = 16 * 1024;

/// Inspect exactly one known candidate. The format is deliberately fixed and
/// returns only the three labels Bosn needs to prove its own ownership.
pub(crate) async fn inspect_setup_gc_container(
    engine: &DockerEngine,
    candidate: &bosn_registry::SetupGcCandidate,
) -> Result<Option<bool>, Error> {
    let format = "{{.Name}}\t{{.State.Running}}\t{{index .Config.Labels \"com.zackees.bosn.setup-managed\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-content-sha256\"}}\t{{index .Config.Labels \"com.zackees.bosn.setup-container\"}}";
    let result = engine
        .with_args(["container", "inspect", "--format", format, &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("setup gc container inspection failed"))?;
    if result.reports_missing() {
        return Ok(None);
    }
    if !result.ok() {
        return Err(Error::Protocol("setup gc container inspection failed"));
    }
    let output = std::str::from_utf8(&result.stdout)
        .map_err(|_| Error::Protocol("setup gc container inspection invalid"))?;
    let fields: Vec<_> = output.trim_end_matches(['\r', '\n']).split('\t').collect();
    let content = candidate
        .generation
        .strip_prefix("sha256:")
        .ok_or(Error::Protocol("setup gc candidate identity invalid"))?;
    if fields.len() != 5
        || fields[0] != format!("/{}", candidate.name)
        || fields[2] != "v1"
        || fields[3] != content
        || fields[4] != candidate.name
    {
        return Err(Error::Protocol("setup gc container ownership mismatch"));
    }
    let running = match fields[1] {
        "true" => true,
        "false" => false,
        _ => return Err(Error::Protocol("setup gc container inspection invalid")),
    };
    Ok(Some(running))
}

pub(crate) async fn apply_setup_gc_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<SetupGcApplyResult, Error> {
    let (id, name, generation) = parse_setup_gc_token(&token)?;
    let candidate = actor
        .setup_gc_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol("setup gc preview is stale or protected"))?;
    let engine = DockerEngine::docker();
    let first = inspect_setup_gc_container(&engine, &candidate).await?;
    if first.is_none() {
        let reconciled = actor
            .finalize_setup_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return reconciled
            .then_some(SetupGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("setup gc preview became stale"));
    }
    if first != Some(false) {
        return Err(Error::Protocol("setup gc candidate is still running"));
    }
    // A second ownership inspection closes the only practical inspect/remove
    // interval without ever using a name glob or Docker selector.
    let second = inspect_setup_gc_container(&engine, &candidate).await?;
    if second.is_none() {
        let reconciled = actor
            .finalize_setup_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return reconciled
            .then_some(SetupGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("setup gc preview became stale"));
    }
    if second != Some(false) {
        return Err(Error::Protocol("setup gc candidate is still running"));
    }
    let removed = engine
        .with_args(["container", "rm", &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("setup gc container removal failed"))?;
    if !removed.ok() {
        return Err(Error::Protocol("setup gc container removal failed"));
    }
    let finalized = actor
        .finalize_setup_gc(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
            false,
        )
        .await?;
    finalized
        .then_some(SetupGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
        .ok_or(Error::Protocol(
            "setup gc registry finalization failed after container removal",
        ))
}

/// Fixed inspection of a daemon-derived volume. Docker has no volume
/// attachment field, so the caller separately uses an exact `ps --filter
/// volume=NAME` query; any attached container (Bosn or foreign) protects it.
pub(crate) async fn inspect_manifest_volume_gc(
    engine: &DockerEngine,
    candidate: &bosn_registry::ManifestVolumeGcCandidate,
) -> Result<Option<bool>, Error> {
    let format = "{{.Name}}\t{{index .Labels \"com.zackees.bosn.setup-managed\"}}\t{{index .Labels \"com.zackees.bosn.setup-content-sha256\"}}\t{{index .Labels \"com.zackees.bosn.setup-container\"}}";
    let result = engine
        .with_args(["volume", "inspect", "--format", format, &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("manifest volume gc inspection failed"))?;
    if result.reports_missing() {
        return Ok(None);
    }
    if !result.ok() {
        return Err(Error::Protocol("manifest volume gc inspection failed"));
    }
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| Error::Protocol("manifest volume gc inspection invalid"))?;
    let fields: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
    let content = candidate
        .generation
        .strip_prefix("sha256:")
        .ok_or(Error::Protocol(
            "manifest volume gc candidate identity invalid",
        ))?;
    if fields.len() != 4
        || fields[0] != candidate.name
        || fields[1] != "v1"
        || fields[2] != content
        || fields[3] != candidate.name
    {
        return Err(Error::Protocol("manifest volume gc ownership mismatch"));
    }
    let attached = engine
        .with_args([
            "container",
            "ls",
            "-a",
            "--filter",
            &format!("volume={}", candidate.name),
            "--format",
            "{{.ID}}",
        ])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("manifest volume gc attachment inspection failed"))?;
    if !attached.ok() {
        return Err(Error::Protocol(
            "manifest volume gc attachment inspection failed",
        ));
    }
    Ok(Some(!attached.stdout.iter().all(u8::is_ascii_whitespace)))
}

pub(crate) async fn apply_manifest_volume_gc_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<ManifestVolumeGcApplyResult, Error> {
    let (id, name, generation) = parse_manifest_volume_gc_token(&token)?;
    let candidate = actor
        .manifest_volume_gc_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol(
            "manifest volume gc preview is stale or protected",
        ))?;
    let engine = DockerEngine::docker();
    let first = inspect_manifest_volume_gc(&engine, &candidate).await?;
    if first.is_none() {
        let finalized = actor
            .finalize_manifest_volume_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return finalized
            .then_some(ManifestVolumeGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("manifest volume gc preview became stale"));
    }
    if first != Some(false) {
        return Err(Error::Protocol("manifest volume gc candidate is attached"));
    }
    let second = inspect_manifest_volume_gc(&engine, &candidate).await?;
    if second.is_none() {
        let finalized = actor
            .finalize_manifest_volume_gc(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?;
        return finalized
            .then_some(ManifestVolumeGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol("manifest volume gc preview became stale"));
    }
    if second != Some(false) {
        return Err(Error::Protocol("manifest volume gc candidate is attached"));
    }
    let removed = engine
        .with_args(["volume", "rm", &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("manifest volume gc removal failed"))?;
    if !removed.ok() {
        return Err(Error::Protocol("manifest volume gc removal failed"));
    }
    let finalized = actor
        .finalize_manifest_volume_gc(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
            false,
        )
        .await?;
    finalized
        .then_some(ManifestVolumeGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
        .ok_or(Error::Protocol(
            "manifest volume gc registry finalization failed after volume removal",
        ))
}

/// The explicit durable path intentionally shares the fixed engine proof with
/// automatic volume GC but has its own registry predicate and opaque token
/// namespace.  A durable row can therefore never become removable merely by
/// entering the GC API.
pub(crate) async fn apply_manifest_volume_release_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<ManifestVolumeGcApplyResult, Error> {
    let (id, name, generation) = parse_manifest_volume_release_token(&token)?;
    let candidate = actor
        .manifest_volume_release_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol(
            "manifest volume release preview is stale or protected",
        ))?;
    let engine = DockerEngine::docker();
    let first = inspect_manifest_volume_gc(&engine, &candidate).await?;
    if first.is_none() {
        return actor
            .finalize_manifest_volume_release(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?
            .then_some(ManifestVolumeGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol(
                "manifest volume release preview became stale",
            ));
    }
    if first != Some(false) {
        return Err(Error::Protocol(
            "manifest volume release candidate is attached",
        ));
    }
    let second = inspect_manifest_volume_gc(&engine, &candidate).await?;
    if second.is_none() {
        return actor
            .finalize_manifest_volume_release(
                workspace,
                candidate.id,
                candidate.name,
                candidate.generation,
                true,
            )
            .await?
            .then_some(ManifestVolumeGcApplyResult {
                removed: false,
                reconciled_missing: true,
            })
            .ok_or(Error::Protocol(
                "manifest volume release preview became stale",
            ));
    }
    if second != Some(false) {
        return Err(Error::Protocol(
            "manifest volume release candidate is attached",
        ));
    }
    // Re-run the complete durable predicate after the final engine proof and
    // immediately before the only destructive Docker command. This closes a
    // lease/session/intent or shared-use race without accepting a fresh name.
    let candidate = actor
        .manifest_volume_release_candidate(
            workspace.clone(),
            candidate.id,
            candidate.name,
            candidate.generation,
        )
        .await?
        .ok_or(Error::Protocol(
            "manifest volume release preview became stale",
        ))?;
    let removed = engine
        .with_args(["volume", "rm", &candidate.name])
        .capture_async(RunOptions::bounded(
            SETUP_GC_ENGINE_DEADLINE,
            SETUP_GC_ENGINE_OUTPUT,
        ))
        .await
        .map_err(|_| Error::Protocol("manifest volume release removal failed"))?;
    if !removed.ok() {
        return Err(Error::Protocol("manifest volume release removal failed"));
    }
    actor
        .finalize_manifest_volume_release(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
            false,
        )
        .await?
        .then_some(ManifestVolumeGcApplyResult {
            removed: true,
            reconciled_missing: false,
        })
        .ok_or(Error::Protocol(
            "manifest volume release registry finalization failed after volume removal",
        ))
}

/// Repair only the durable lifecycle accounting for one previewed setup app
/// that fixed Docker inspection proves absent. No Docker mutation occurs: the
/// next semantic ensure is solely responsible for creating/re-recording an
/// app. A forged/stale token is rejected before inspection because the actor
/// first proves the exact active ownership shape.
pub(crate) async fn repair_missing_setup_reconcile_candidate(
    actor: &RegistryActor,
    reconcile: &dyn SetupReconcileExecutor,
    workspace: String,
    token: String,
) -> Result<SetupReconcileMissingRepairResult, Error> {
    let (id, name, generation) = parse_setup_reconcile_missing_token(&token)?;
    if !actor
        .setup_missing_repair_candidate(
            workspace.clone(),
            id.clone(),
            name.clone(),
            generation.clone(),
        )
        .await?
    {
        // A repeat of a successful exact token is safe and silent. All other
        // stale/protected shapes fail closed without Docker observation.
        return match actor
            .repair_missing_setup_container(workspace, id, name, generation)
            .await?
        {
            Some(bosn_registry::SetupMissingRepair::AlreadyRepaired) => {
                Ok(SetupReconcileMissingRepairResult {
                    repaired: false,
                    already_repaired: true,
                })
            }
            _ => Err(Error::Protocol(
                "setup reconcile repair preview is stale or protected",
            )),
        };
    }
    match reconcile.inspect(&name).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(Error::Protocol(
                "setup reconcile candidate is no longer missing",
            ));
        }
        Err(_) => {
            return Err(Error::Protocol(
                "setup reconcile container inspection failed",
            ));
        }
    }
    match actor
        .repair_missing_setup_container(workspace, id, name, generation)
        .await?
    {
        Some(bosn_registry::SetupMissingRepair::Repaired) => {
            Ok(SetupReconcileMissingRepairResult {
                repaired: true,
                already_repaired: false,
            })
        }
        Some(bosn_registry::SetupMissingRepair::AlreadyRepaired) => {
            Ok(SetupReconcileMissingRepairResult {
                repaired: false,
                already_repaired: true,
            })
        }
        None => Err(Error::Protocol(
            "setup reconcile repair preview became stale",
        )),
    }
}

/// Stop a live retired candidate with no caller-controlled Docker input. A
/// subsequent fixed inspection proves it transitioned to stopped before the
/// actor records the event. The registry record is intentionally retained.
pub(crate) async fn stop_setup_retired_candidate(
    actor: &RegistryActor,
    workspace: String,
    token: String,
) -> Result<SetupRetiredStopResult, Error> {
    let (id, name, generation) = parse_setup_gc_token(&token)?;
    stop_retired_container(actor, workspace, id, name, generation)
        .await?
        .ok_or(Error::Protocol("setup retired stop candidate is absent"))
}

/// What [`stop_retired_stack_containers`] did, by container name.
#[derive(Debug, Default)]
pub(crate) struct RetiredStackStops {
    pub(crate) stopped: Vec<String>,
    /// A candidate that could not be stopped, with the reason.
    pub(crate) failed: Vec<(String, Error)>,
}

/// Stop every retired container of one manifest stack that no task runs in
/// (#383). Stack- and machine-scoped volumes outlive a generation, so a
/// retired container's daemons (soldr-broker in `/root/.soldr`) would
/// otherwise keep serving the shared volumes to the new generation from a
/// different mount namespace. The selection is the explicit retired-stop's:
/// an execution session or lease still protects a container, and its last
/// task stops it on finishing.
pub(crate) async fn stop_retired_stack_containers(
    actor: &RegistryActor,
    workspace: &str,
    stack: &str,
) -> Result<RetiredStackStops, Error> {
    let prefix = setup_container_resource_id("manifest-container", stack, "");
    let mut retired = Vec::new();
    let mut after = 0;
    loop {
        let page = actor
            .setup_gc_preview(workspace.to_owned(), after, MAX_REGISTRY_DIAGNOSTIC_PAGE)
            .await?;
        retired.extend(
            page.candidates
                .into_iter()
                .filter(|candidate| candidate.id.starts_with(&prefix)),
        );
        match page.next {
            Some(next) => after = next,
            None => break,
        }
    }
    let mut stops = RetiredStackStops::default();
    for candidate in retired {
        let name = candidate.name.clone();
        match stop_retired_container(
            actor,
            workspace.to_owned(),
            candidate.id,
            candidate.name,
            candidate.generation,
        )
        .await
        {
            Ok(Some(result)) if result.stopped => stops.stopped.push(name),
            // Already stopped, or removed outside Bosn: nothing runs in it.
            Ok(_) => {}
            Err(error) => stops.failed.push((name, error)),
        }
    }
    Ok(stops)
}

/// Recheck one retired candidate against the registry and Docker, then stop
/// it; `None` when Docker no longer has it. The registry record is retained
/// for GC.
async fn stop_retired_container(
    actor: &RegistryActor,
    workspace: String,
    id: String,
    name: String,
    generation: String,
) -> Result<Option<SetupRetiredStopResult>, Error> {
    let candidate = actor
        .setup_gc_candidate(workspace.clone(), id, name, generation)
        .await?
        .ok_or(Error::Protocol(
            "setup retired stop preview is stale or protected",
        ))?;
    let engine = DockerEngine::docker();
    match inspect_setup_gc_container(&engine, &candidate).await? {
        None => return Ok(None),
        Some(false) => {
            // Already stopped is idempotent. Do not add duplicate events and
            // keep the exact retired candidate eligible for explicit GC.
            return Ok(Some(SetupRetiredStopResult {
                stopped: false,
                already_stopped: true,
            }));
        }
        Some(true) => {}
    }
    // Recheck the exact identity after registry selection, then make the one
    // permitted engine mutation. The short grace is product-fixed and stays
    // inside the absolute engine deadline; no caller-controlled argv or
    // timeout reaches this boundary.
    if inspect_setup_gc_container(&engine, &candidate).await? != Some(true) {
        return Err(Error::Protocol("setup retired stop candidate changed"));
    }
    // A macOS guest contains a live VM disk. Its typed create shape installs
    // a 120-second stop timeout so QEMU can flush; preserving that timeout
    // here prevents the explicit retired-stop operation from silently
    // degrading into the normal short-lived setup-app policy.
    let (stop_seconds, stop_deadline) = if candidate.id.starts_with("manifest-guest:") {
        ("120", Duration::from_secs(150))
    } else {
        ("1", SETUP_GC_ENGINE_DEADLINE)
    };
    let stopped = engine
        .with_args(["container", "stop", "--time", stop_seconds, &candidate.name])
        .capture_async(RunOptions::bounded(stop_deadline, SETUP_GC_ENGINE_OUTPUT))
        .await
        .map_err(|_| Error::Protocol("setup retired stop failed"))?;
    if !stopped.ok() {
        return Err(Error::Protocol("setup retired stop failed"));
    }
    if inspect_setup_gc_container(&engine, &candidate).await? != Some(false) {
        return Err(Error::Protocol("setup retired stop did not stop candidate"));
    }
    let recorded = actor
        .confirm_setup_retired_stopped(
            workspace,
            candidate.id,
            candidate.name,
            candidate.generation,
        )
        .await?;
    if !recorded {
        return Err(Error::Protocol("setup retired stop registry became stale"));
    }
    Ok(Some(SetupRetiredStopResult {
        stopped: true,
        already_stopped: false,
    }))
}

#[cfg(all(test, unix))]
#[path = "gc_apply_tests.rs"]
mod tests;
