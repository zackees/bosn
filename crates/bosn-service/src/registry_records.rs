//! Registry transactions behind the actor: setup/manifest ensure records, sessions, adoption, events.

use super::*;

pub(crate) fn setup_app_task_session_id(job_id: u64) -> String {
    format!("setup-app-task:{job_id}")
}
pub(crate) fn manifest_app_task_session_id(job_id: u64) -> String {
    format!("manifest-app-task:{job_id}")
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
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        // Do not remove the session: cancelling/timing out the local Docker
        // client does not prove the remote `exec` process ended.
        transaction.append_event(now, "setup.app-task.uncertain", "remote_completion_unknown")?;
    } else {
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
    let mut transaction = registry.begin_immediate()?;
    if outcome == "uncertain" {
        transaction.append_event(
            now,
            "manifest.app-task.uncertain",
            "remote_completion_unknown",
        )?;
    } else {
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
    transaction.put_resource(&Resource {
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
        retention: Retention::Pinned,
    })?;
    transaction.put_resource_use(&ResourceUse {
        resource_id: container.id.clone(),
        workspace: container.workspace.clone(),
        stack: container.stack.clone(),
        generation: container.generation.clone(),
        last_used: now,
        state: ResourceState::Active,
    })?;
    transaction.put_resource(&Resource {
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
        retention: Retention::Pinned,
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
        transaction.put_resource(&Resource {
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
            retention: Retention::Pinned,
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
                || existing.state != ResourceState::Active
                || existing.retention != Retention::Pinned)
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
        tx.put_resource(&Resource {
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
            retention: Retention::Pinned,
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
