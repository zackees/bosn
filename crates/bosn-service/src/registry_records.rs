//! Registry transactions behind the actor: setup/manifest ensure records, sessions, adoption, events.

use super::*;

// Setup and manifest container and image rows are written `warm` (#545): nothing pins an app
// container or its image, so a `pinned` row was bookkeeping, not a promise. Liveness, leases and
// execution sessions protect them. A row someone did pin stays pinned.

pub(crate) fn setup_app_task_session_id(job_id: u64) -> String {
    format!("setup-app-task:{job_id}")
}
pub(crate) fn manifest_app_task_session_id(job_id: u64) -> String {
    format!("manifest-app-task:{job_id}")
}

/// The resources a finished app task used, with `last_used` moved to `now` (#545).
///
/// A keepalive container used only through `exec` would otherwise age from its last ensure and be
/// stopped mid-use cadence. The task's container and every resource of its exact workspace and
/// stack (its image and volumes) restart their idle clock at completion. Retention is untouched.
fn completed_session_resources(
    registry: &Registry,
    session_id: &str,
    now: f64,
) -> Result<Vec<Resource>, bosn_registry::Error> {
    let mut container_name = None;
    let mut offset = 0;
    loop {
        let page = registry.execution_sessions(offset, 64)?;
        if let Some(session) = page
            .items
            .into_iter()
            .find(|session| session.id == session_id)
        {
            container_name = Some(session.container_id);
            break;
        }
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    let Some(name) = container_name else {
        return Ok(Vec::new());
    };
    let Some(container) = registry.resource_by_kind_name(ResourceKind::Container, &name)? else {
        return Ok(Vec::new());
    };
    let mut touched = Vec::new();
    offset = 0;
    loop {
        let page = registry.resources(offset, 64)?;
        touched.extend(page.items.into_iter().filter_map(|mut resource| {
            (resource.workspace == container.workspace && resource.stack == container.stack).then(
                || {
                    resource.last_used = resource.last_used.max(now);
                    resource
                },
            )
        }));
        let Some(next) = page.next_offset else { break };
        offset = next;
    }
    Ok(touched)
}

pub(crate) fn record_setup_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    container_id: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    transaction.put_execution_session(&ExecutionSession {
        id: setup_app_task_session_id(job_id),
        container_id: container_id.into(),
        engine_binary: "docker".into(),
        client_pid: std::process::id(),
        client_start: None,
        lease_ids: Vec::new(),
    })?;
    transaction.append_event(now, "setup.app-task.started", "owned_declared_task")?;
    transaction.commit()
}

pub(crate) fn finish_setup_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    outcome: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let touched = completed_session_resources(registry, &setup_app_task_session_id(job_id), now)?;
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        // Do not remove the session: cancelling/timing out the local Docker
        // client does not prove the remote `exec` process ended.
        transaction.append_event(now, "setup.app-task.uncertain", "remote_completion_unknown")?;
    } else {
        for resource in &touched {
            transaction.put_resource(resource)?;
        }
        transaction.delete_execution_session(&setup_app_task_session_id(job_id))?;
        transaction.append_event(now, "setup.app-task.finished", outcome)?;
    }
    transaction.commit()
}

pub(crate) fn record_manifest_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    container_id: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    transaction.put_execution_session(&ExecutionSession {
        id: manifest_app_task_session_id(job_id),
        container_id: container_id.into(),
        engine_binary: "docker".into(),
        client_pid: std::process::id(),
        client_start: None,
        lease_ids: Vec::new(),
    })?;
    transaction.append_event(now, "manifest.app-task.started", "owned_declared_task")?;
    transaction.commit()
}
pub(crate) fn finish_manifest_app_task_session(
    registry: &mut Registry,
    job_id: u64,
    outcome: &str,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let touched =
        completed_session_resources(registry, &manifest_app_task_session_id(job_id), now)?;
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        transaction.append_event(
            now,
            "manifest.app-task.uncertain",
            "remote_completion_unknown",
        )?;
    } else {
        for resource in &touched {
            transaction.put_resource(resource)?;
        }
        transaction.delete_execution_session(&manifest_app_task_session_id(job_id))?;
        transaction.append_event(now, "manifest.app-task.finished", outcome)?;
    }
    transaction.commit()
}

pub(crate) fn record_setup_ensure(
    registry: &mut Registry,
    job_id: u64,
    execution: &SetupEnsureExecution,
) -> Result<(), bosn_registry::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    let container = &execution.resource;
    let image = &execution.image;
    transaction.put_resource_preserving_pin(&Resource {
        id: container.id.clone(),
        kind: ResourceKind::Container,
        name: container.name.clone(),
        stack: container.stack.clone(),
        generation: container.generation.clone(),
        // Setup app container names are machine-global and content-addressed.
        scope: Scope::Machine,
        workspace: container.workspace.clone(),
        created_at: now,
        last_used: now,
        state: ResourceState::Active,
        retention: Retention::Warm,
    })?;
    transaction.put_resource_use(&ResourceUse {
        resource_id: container.id.clone(),
        workspace: container.workspace.clone(),
        stack: container.stack.clone(),
        generation: container.generation.clone(),
        last_used: now,
        state: ResourceState::Active,
    })?;
    transaction.put_resource_preserving_pin(&Resource {
        id: image.id.clone(),
        kind: ResourceKind::Image,
        name: image.name.clone(),
        stack: image.stack.clone(),
        generation: image.generation.clone(),
        // The inspected image ID identifies a machine-local Docker image.
        scope: Scope::Machine,
        workspace: image.workspace.clone(),
        created_at: now,
        last_used: now,
        state: ResourceState::Active,
        retention: Retention::Warm,
    })?;
    transaction.put_resource_use(&ResourceUse {
        resource_id: image.id.clone(),
        workspace: image.workspace.clone(),
        stack: image.stack.clone(),
        generation: image.generation.clone(),
        last_used: now,
        state: ResourceState::Active,
    })?;
    // A new successful setup document generation supersedes only prior Bosn
    // setup *container* ownership in this exact canonical workspace/stack.
    // It does not stop, delete, or otherwise mutate Docker; it also leaves
    // image ownership active because inspected image identities can be shared
    // across documents and workspaces. Keeping this after both current
    // resource upserts means an image conflict rolls back without retiring a
    // previously active generation.
    transaction.retire_prior_setup_container_generations(
        &container.workspace,
        &container.stack,
        &container.generation,
    )?;
    // Success is never visible in the event log until both durable ownership
    // facts and any generation retirement have been accepted by this very
    // transaction.
    let event = SetupEnsureEvent::terminal(job_id, SetupEnsureEventOutcome::Succeeded);
    transaction.append_event(now, event.kind, &event.detail)?;
    transaction.commit()
}

/// Persist one successful manifest-runtime ensure atomically. A succeeding
/// generation is recorded before only the previous manifest container use for
/// this exact workspace/stack is retired. This is durable lifecycle accounting
/// only: it never stops/deletes a container or image, and a failed upsert rolls
/// the whole transition back without retiring the prior generation.
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) fn record_manifest_ensure(
    registry: &mut Registry,
    job_id: u64,
    execution: &SetupEnsureExecution,
    contract: &ManifestRecoveryContract,
) -> Result<(), bosn_registry::Error> {
    if contract.resource_id != execution.resource.id
        || contract.name != execution.resource.name
        || contract.workspace != execution.resource.workspace
        || contract.stack != execution.resource.stack
        || contract.generation != execution.resource.generation
        || contract.image_identity != execution.image.generation
        || contract.guest != execution.resource.id.starts_with("manifest-guest:")
        || !safe_manifest_relative_path(&contract.manifest)
    {
        return Err(bosn_registry::Error::BadRow("manifest recovery contract"));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &execution.resource.id,
            &execution.resource.name,
            &execution.resource.stack,
            &execution.resource.generation,
            &execution.resource.workspace,
        ),
        (
            ResourceKind::Image,
            &execution.image.id,
            &execution.image.name,
            &execution.image.stack,
            &execution.image.generation,
            &execution.image.workspace,
        ),
    ] {
        transaction.put_resource_preserving_pin(&Resource {
            id: id.clone(),
            kind,
            name: name.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            scope: Scope::Machine,
            workspace: workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: Retention::Warm,
        })?;
        transaction.put_resource_use(&ResourceUse {
            resource_id: id.clone(),
            workspace: workspace.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
    }
    // The engine volume was created/reused only after its exact contract was
    // durably intended.  Record the resource and consume that intent in the
    // same transaction as container success; normal generation rollover never
    // deletes or retires volume data.
    for volume in &execution.volumes {
        transaction.put_resource(&Resource {
            id: volume.id.clone(),
            kind: ResourceKind::Volume,
            name: volume.name.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            scope: volume.scope,
            workspace: volume.workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: volume.retention,
        })?;
        transaction.put_resource_use(&ResourceUse {
            resource_id: volume.id.clone(),
            workspace: volume.workspace.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
        transaction.delete_volume_creation_intent(&volume.name)?;
    }
    // This follows both upserts so an image/container identity conflict drops
    // the transaction with the preceding generation still active. The
    // registry primitive is manifest-namespace-only and never changes setup
    // resources, images, other stacks, or other workspaces.
    transaction.retire_prior_manifest_container_generations(
        &execution.resource.workspace,
        &execution.resource.stack,
        &execution.resource.generation,
    )?;
    transaction.retire_prior_manifest_warm_spec_volume_generations(
        &execution.resource.workspace,
        &execution.resource.stack,
        &execution
            .volumes
            .iter()
            .map(|volume| volume.name.clone())
            .collect::<Vec<_>>(),
    )?;
    transaction.append_event(
        now,
        "manifest.recovery.contract",
        &manifest_recovery_contract_json(contract),
    )?;
    transaction.append_event(
        now,
        if contract.autostart {
            "manifest.autostart.intent_recorded"
        } else {
            "manifest.autostart.not_selected"
        },
        &manifest_autostart_intent_detail(contract),
    )?;
    transaction.append_event(
        now,
        "manifest.ensure.succeeded",
        &format!("job_id={job_id}"),
    )?;
    transaction.commit()
}

pub(crate) fn put_manifest_volume_intents(
    registry: &mut Registry,
    volumes: &[ManifestVolumeResource],
) -> Result<(), bosn_registry::Error> {
    let mut transaction = registry.begin_immediate()?;
    for volume in volumes {
        transaction.put_volume_creation_intent(&VolumeCreationIntent {
            name: volume.name.clone(),
            labels: volume.labels.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            scope: volume.scope,
            workspace: volume.workspace.clone(),
        })?;
    }
    transaction.commit()
}

/// Restore only an absent registry view of an already proven Docker fact.
/// Existing records must exactly agree with the re-derived ownership facts;
/// adoption is never an overwrite or a way to cross workspace/stack state.
pub(crate) fn record_setup_adoption(
    registry: &mut Registry,
    execution: &SetupEnsureExecution,
) -> Result<(), bosn_registry::Error> {
    let container = &execution.resource;
    let image = &execution.image;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &container.id,
            &container.name,
            &container.stack,
            &container.generation,
            &container.workspace,
        ),
        (
            ResourceKind::Image,
            &image.id,
            &image.name,
            &image.stack,
            &image.generation,
            &image.workspace,
        ),
    ] {
        if let Some(existing) = registry.resource_by_kind_name(kind, name)?
            && (existing.id != *id
                || existing.stack != *stack
                || existing.generation != *generation
                || existing.workspace != *workspace
                || existing.scope != Scope::Machine
                || existing.state != ResourceState::Active)
        {
            return Err(bosn_registry::Error::ResourceIdentityConflict);
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut tx = registry.begin_immediate()?;
    for (kind, id, name, stack, generation, workspace) in [
        (
            ResourceKind::Container,
            &container.id,
            &container.name,
            &container.stack,
            &container.generation,
            &container.workspace,
        ),
        (
            ResourceKind::Image,
            &image.id,
            &image.name,
            &image.stack,
            &image.generation,
            &image.workspace,
        ),
    ] {
        tx.put_resource_preserving_pin(&Resource {
            id: id.clone(),
            kind,
            name: name.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            scope: Scope::Machine,
            workspace: workspace.clone(),
            created_at: now,
            last_used: now,
            state: ResourceState::Active,
            retention: Retention::Warm,
        })?;
        tx.put_resource_use(&ResourceUse {
            resource_id: id.clone(),
            workspace: workspace.clone(),
            stack: stack.clone(),
            generation: generation.clone(),
            last_used: now,
            state: ResourceState::Active,
        })?;
    }
    tx.append_event(now, "setup.ensure.adopted", "managed_setup_app_restored")?;
    tx.commit()
}

pub(crate) fn append_setup_ensure_events(
    registry: &mut Registry,
    events: &[SetupEnsureEvent],
) -> Result<(), bosn_registry::Error> {
    if events.is_empty() {
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    for event in events {
        transaction.append_event(now, event.kind, &event.detail)?;
    }
    transaction.commit()
}

pub(crate) fn append_manifest_recovery_events(
    registry: &mut Registry,
    events: &[(String, String)],
) -> Result<(), bosn_registry::Error> {
    if events.is_empty() {
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
        .as_secs_f64();
    let mut transaction = registry.begin_immediate()?;
    for (kind, detail) in events {
        if !(kind.starts_with("manifest.recovery.") || kind.starts_with("manifest.autostart."))
            || kind.len() > 128
            || detail.len() > 1024
        {
            return Err(bosn_registry::Error::BadRow("manifest recovery event"));
        }
        transaction.append_event(now, kind, detail)?;
    }
    transaction.commit()
}

/// Select the durable registry key from a receipt that has already passed
/// `adopt_setup_app`'s complete ownership validation. Docker's opaque ID is
/// useful in the operation receipt, but registry resource ownership and GC
/// use the exact content-addressed managed name.
pub(crate) fn setup_app_task_session_container_identity(observed: &SetupEnsureResult) -> String {
    observed.container_name.clone()
}
pub(crate) fn manifest_app_task_session_container_identity(observed: &SetupEnsureResult) -> String {
    observed.container_name.clone()
}
