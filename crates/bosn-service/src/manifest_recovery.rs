//! Manifest recovery contracts and daemon-start restart of stopped manifest apps.

use super::*;

pub(crate) fn manifest_recovery_contract(
    request: &ManifestEnsureJobRequest,
    execution: &SetupEnsureExecution,
    job_id: u64,
) -> Result<ManifestRecoveryContract, String> {
    let resource = &execution.resource;
    if !safe_manifest_relative_path(&request.manifest)
        || resource.stack != request.stack
        || !resource.generation.starts_with("sha256:")
        || !matches!(resource.id.as_str(), value if value.starts_with("manifest-container:") || value.starts_with("manifest-guest:"))
        || execution.image.generation.is_empty()
        || execution.image.generation.len() > 1024
    {
        return Err("manifest recovery contract cannot be derived".into());
    }
    Ok(ManifestRecoveryContract {
        resource_id: resource.id.clone(),
        name: resource.name.clone(),
        workspace: resource.workspace.clone(),
        stack: resource.stack.clone(),
        generation: resource.generation.clone(),
        manifest: request.manifest.clone(),
        image_identity: execution.image.generation.clone(),
        guest: resource.id.starts_with("manifest-guest:"),
        intent_id: format!("job-{job_id}"),
        autostart: execution.manifest_autostart,
    })
}

pub(crate) fn manifest_recovery_contract_json(contract: &ManifestRecoveryContract) -> String {
    serde_json::json!({
        "v": 2,
        "resource_id": contract.resource_id,
        "name": contract.name,
        "workspace": contract.workspace,
        "stack": contract.stack,
        "generation": contract.generation,
        "manifest": contract.manifest,
        "image_identity": contract.image_identity,
        "guest": contract.guest,
        "intent_id": contract.intent_id,
        "autostart": contract.autostart,
    })
    .to_string()
}

/// Exact durable veto key for one immutable successful desired-state record.
/// It is deliberately generated only from daemon-written contract fields and
/// queried with SQLite equality, not a prefix/substring match. This leaves
/// v5 registry schema stable while making a source/policy veto survive daemon
/// restarts. A subsequent successful ensure carries a new `intent_id`.
pub(crate) fn manifest_autostart_intent_detail(contract: &ManifestRecoveryContract) -> String {
    serde_json::json!({
        "v": 1,
        "resource_id": contract.resource_id,
        "generation": contract.generation,
        "intent_id": contract.intent_id,
    })
    .to_string()
}

pub(crate) fn parse_manifest_recovery_contract(detail: &str) -> Option<ManifestRecoveryContract> {
    let value: serde_json::Value = serde_json::from_str(detail).ok()?;
    let object = value.as_object()?;
    let field = |name: &str| object.get(name)?.as_str().map(str::to_owned);
    if object.get("v")?.as_u64()? != 2 {
        return None;
    }
    let contract = ManifestRecoveryContract {
        resource_id: field("resource_id")?,
        name: field("name")?,
        workspace: field("workspace")?,
        stack: field("stack")?,
        generation: field("generation")?,
        manifest: field("manifest")?,
        image_identity: field("image_identity")?,
        guest: object.get("guest")?.as_bool()?,
        intent_id: field("intent_id")?,
        autostart: object.get("autostart")?.as_bool()?,
    };
    (contract.resource_id.len() <= 512
        && contract.name.len() <= 512
        && contract.workspace.len() <= 4096
        && contract.stack.len() <= 256
        && contract.image_identity.len() <= 1024
        && contract.intent_id.len() <= 64
        && contract.intent_id.starts_with("job-")
        && contract.intent_id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        && safe_manifest_relative_path(&contract.manifest)
        && contract.generation.starts_with("sha256:")
        && ((contract.guest && contract.resource_id.starts_with("manifest-guest:"))
            || (!contract.guest && contract.resource_id.starts_with("manifest-container:"))))
    .then_some(contract)
}

pub(crate) fn classify_manifest_recovery_observation(
    contract: &ManifestRecoveryContract,
    observed: &SetupReconcileObserved,
) -> Option<&'static str> {
    let content = contract.generation.strip_prefix("sha256:")?;
    if observed.name != format!("/{}", contract.name) {
        return None;
    }
    if observed.managed != "v1"
        || observed.content != content
        || observed.container != contract.name
        || observed.image_identity != contract.image_identity
    {
        return None;
    }
    Some(if observed.running {
        "running"
    } else {
        "stopped"
    })
}

/// Daemon-start restart policy for native manifest containers. A contract is
/// only an initial pointer: every candidate is re-derived from the current
/// bounded manifest, authorized from active registry rows (which also reject
/// dangling volume intents and uncertain task sessions), then inspected for
/// exact identity before a fixed `container start`. We intentionally do not
/// create a missing object, repair registry state, search Docker, or continue
/// when a source path/build context is gone or changed.
pub(crate) async fn recover_manifest_startup(
    actor: &RegistryActor,
    state_dir: &Path,
    executor: &dyn ManifestRecoveryExecutor,
) -> Result<(), Error> {
    let details = actor.manifest_recovery_contracts().await?;
    let mut seen = std::collections::BTreeSet::new();
    let mut events = Vec::new();
    let deadline = async_engine::Deadline::after(MANIFEST_RECOVERY_TOTAL_DEADLINE);
    for detail in details {
        let Some(contract) = parse_manifest_recovery_contract(&detail) else {
            events.push((
                "manifest.recovery.refused_contract".into(),
                "malformed".into(),
            ));
            continue;
        };
        if !seen.insert(contract.resource_id.clone()) {
            continue;
        }
        // A prior source/policy veto is durable and exact to this successful
        // intent. Do not repeatedly reopen a removed workspace or recreate
        // engine pressure on every daemon launch. Fresh successful ensure is
        // the only path that creates a new intent.
        if !contract.autostart {
            continue;
        }
        if actor.manifest_autostart_intent_disabled(&contract).await? {
            events.push((
                "manifest.autostart.already_disabled".into(),
                manifest_autostart_intent_detail(&contract),
            ));
            continue;
        }
        if deadline.remaining().is_zero() {
            events.push(("manifest.recovery.deadline".into(), "bounded".into()));
            break;
        }
        let request = ManifestEnsureJobRequest {
            workspace: PathBuf::from(&contract.workspace),
            manifest: contract.manifest.clone(),
            stack: contract.stack.clone(),
            deadline: deadline.remaining(),
            output_limit: 4096,
        };
        let runtime = match async_engine::timeout_at(
            deadline,
            manifest_stack_setup_plan_at(&request, Some(state_dir)),
        )
        .await
        {
            Ok(Ok(runtime))
                if runtime.generation == contract.generation
                    && runtime.is_guest == contract.guest
                    && runtime.autostart =>
            {
                runtime
            }
            Ok(Ok(runtime)) if !runtime.autostart => {
                events.push((
                    "manifest.autostart.disabled".into(),
                    manifest_autostart_intent_detail(&contract),
                ));
                events.push((
                    "manifest.recovery.refused_policy".into(),
                    "default_stack_changed".into(),
                ));
                continue;
            }
            Ok(Ok(_)) => {
                events.push((
                    "manifest.autostart.disabled".into(),
                    manifest_autostart_intent_detail(&contract),
                ));
                events.push((
                    "manifest.recovery.refused_source".into(),
                    "generation_mismatch".into(),
                ));
                continue;
            }
            Ok(Err(_)) => {
                events.push((
                    "manifest.autostart.disabled".into(),
                    manifest_autostart_intent_detail(&contract),
                ));
                events.push((
                    "manifest.recovery.refused_source".into(),
                    "unavailable_or_invalid".into(),
                ));
                continue;
            }
            Err(_) => {
                events.push(("manifest.recovery.deadline".into(), "source_proof".into()));
                break;
            }
        };
        // `runtime` is deliberately retained through this check: successful
        // source proof must precede any registry/engine authority.
        let _ = runtime;
        if !actor.manifest_recovery_authorized(contract.clone()).await? {
            events.push((
                "manifest.recovery.refused_registry".into(),
                "inactive_session_or_volume_intent".into(),
            ));
            continue;
        }
        let inspected =
            match async_engine::timeout_at(deadline, executor.inspect(&contract.name)).await {
                Ok(Ok(Some(observed))) => observed,
                Ok(Ok(None)) => {
                    events.push((
                        "manifest.recovery.missing".into(),
                        "exact_container_absent".into(),
                    ));
                    continue;
                }
                Ok(Err(_)) => {
                    events.push(("manifest.recovery.inspect_error".into(), "bounded".into()));
                    continue;
                }
                Err(_) => {
                    events.push(("manifest.recovery.deadline".into(), "inspect".into()));
                    break;
                }
            };
        match classify_manifest_recovery_observation(&contract, &inspected) {
            Some("running") => {
                events.push(("manifest.recovery.already_running".into(), "exact".into()))
            }
            Some("stopped") => {
                match async_engine::timeout_at(deadline, executor.start(&contract.name)).await {
                    Ok(Ok(())) => {
                        match async_engine::timeout_at(deadline, executor.inspect(&contract.name))
                            .await
                        {
                            Ok(Ok(Some(observed)))
                                if classify_manifest_recovery_observation(&contract, &observed)
                                    == Some("running") =>
                            {
                                events.push(("manifest.recovery.started".into(), "exact".into()));
                            }
                            Ok(Ok(_)) => events.push((
                                "manifest.recovery.start_unconfirmed".into(),
                                "post_start_mismatch".into(),
                            )),
                            Ok(Err(_)) => events.push((
                                "manifest.recovery.start_unconfirmed".into(),
                                "post_start_inspect_error".into(),
                            )),
                            Err(_) => events.push((
                                "manifest.recovery.deadline".into(),
                                "post_start_inspect".into(),
                            )),
                        }
                    }
                    Ok(Err(_)) => {
                        events.push(("manifest.recovery.start_error".into(), "bounded".into()))
                    }
                    Err(_) => events.push(("manifest.recovery.deadline".into(), "start".into())),
                }
            }
            _ => events.push((
                "manifest.recovery.refused_engine".into(),
                "identity_mismatch".into(),
            )),
        }
    }
    actor.append_manifest_recovery_events(events).await
}
