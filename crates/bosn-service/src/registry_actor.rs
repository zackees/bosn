//! The registry actor loop: the sole writer, serving typed commands one at a time.

use super::*;

#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "baseline, ci.yml#229"
)]
pub(crate) async fn registry_actor(
    mut registry: Registry,
    mut receiver: async_engine::Receiver<DbCommand>,
    #[cfg(test)] mut setup_ensure_record_gate: Option<SetupEnsureRecordGate>,
) {
    // The sole writer lease fences prior daemon owners. This authority expires
    // before any new Act creation/claim and is never restored by reads.
    let mut act_startup_open = true;
    while let Some(command) = receiver.recv().await {
        match command {
            DbCommand::ActRegistry { command, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        act_registry::apply(&mut registry, *command, &mut act_startup_open);
                    (registry, act_startup_open, result)
                });
                match worker.await {
                    Ok((returned, startup_open, result)) => {
                        registry = returned;
                        act_startup_open = startup_open;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::Status(reply) => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.status().map(Status::from);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::DoctorIntegrity(reply) => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.integrity_check();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(if result.is_ok() { "ready" } else { "failed" });
                    }
                    Err(_) => {
                        let _ = reply.send("unavailable");
                        return;
                    }
                }
            }
            DbCommand::Resources {
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| registry.resources(after, limit as usize))
                        .map(|page| RegistryResourcePage {
                            next: page.next_offset.map(|value| value as u64),
                            records: page.items.into_iter().map(resource_diagnostic).collect(),
                        });
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::SetupEnsureEvents {
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| registry.setup_ensure_events(after, limit as usize))
                        .map(|page| SetupEnsureEventPage {
                            next: page.next_offset.map(|value| value as u64),
                            records: page.items.into_iter().map(event_diagnostic).collect(),
                        });
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::SetupGcPreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| {
                            registry.setup_gc_preview(&workspace, after, limit as usize)
                        })
                        .map(setup_gc_preview_diagnostic);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestVolumeGcPreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| {
                            registry.manifest_volume_gc_preview(&workspace, after, limit as usize)
                        })
                        .map(manifest_volume_gc_preview_diagnostic);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestVolumeReleasePreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = usize::try_from(after)
                        .map_err(|_| bosn_registry::Error::BadRow("page offset"))
                        .and_then(|after| {
                            registry.manifest_volume_release_preview(
                                &workspace,
                                after,
                                limit as usize,
                            )
                        })
                        .map(|page| ManifestVolumeGcPreviewPage {
                            next: page.next_offset.map(|v| v as u64),
                            candidates: page
                                .items
                                .into_iter()
                                .map(|v| ManifestVolumeGcCandidateDiagnostic {
                                    token: manifest_volume_release_token(&v),
                                    id: v.id,
                                    name: v.name,
                                    generation: v.generation,
                                    reason: "explicit_durable_manifest_volume_release".into(),
                                })
                                .collect(),
                            counts: ManifestVolumeGcPreviewCounts::default(),
                        });
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::SetupReconcilePreview {
                workspace,
                after,
                limit,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let after = usize::try_from(after)
                            .map_err(|_| bosn_registry::Error::BadRow("page offset"))?;
                        let containers = registry.setup_reconcile_containers(
                            &workspace,
                            after,
                            limit as usize,
                        )?;
                        // Image IDs are durable facts recorded by successful ensure. A
                        // bounded preview refuses to infer identity from a name/tag.
                        let images = registry
                            .setup_reconcile_images(&workspace)?
                            .items
                            .into_iter()
                            .map(|r| r.generation)
                            .collect::<Vec<_>>();
                        Ok((
                            containers.next_offset.map(|v| v as u64),
                            containers
                                .items
                                .into_iter()
                                .map(|resource| {
                                    let missing_repairable = registry
                                        .setup_missing_repair_candidate(
                                            &workspace,
                                            &resource.id,
                                            &resource.name,
                                            &resource.generation,
                                        )?;
                                    Ok(SetupReconcileCandidate {
                                        resource,
                                        image_identities: images.clone(),
                                        missing_repairable,
                                    })
                                })
                                .collect::<Result<Vec<_>, bosn_registry::Error>>()?,
                        ))
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::RepairMissingSetupContainer {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let repaired = transaction.repair_missing_setup_container(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                        )?;
                        if repaired == Some(bosn_registry::SetupMissingRepair::Repaired) {
                            transaction.commit()?;
                        }
                        Ok(repaired)
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::SetupMissingRepairCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.setup_missing_repair_candidate(
                        &workspace,
                        &id,
                        &name,
                        &generation,
                    );
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::SetupGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.setup_gc_candidate(&workspace, &id, &name, &generation);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestVolumeGcCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        registry.manifest_volume_gc_candidate(&workspace, &id, &name, &generation);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestVolumeReleaseCandidate {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.manifest_volume_release_candidate(
                        &workspace,
                        &id,
                        &name,
                        &generation,
                    );
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::FinalizeSetupGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let kind = if missing {
                            "setup.gc.reconciled_missing"
                        } else {
                            "setup.gc.removed"
                        };
                        let removed = transaction.finalize_setup_gc_candidate(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                            kind,
                        )?;
                        // Dropping an uncommitted immediate transaction rolls it
                        // back; no stale-preview event is persisted.
                        if removed {
                            transaction.commit()?;
                        }
                        Ok(removed)
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::FinalizeManifestVolumeGc {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let removed = transaction.finalize_manifest_volume_gc_candidate(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                            if missing {
                                "manifest.volume_gc.reconciled_missing"
                            } else {
                                "manifest.volume_gc.removed"
                            },
                        )?;
                        if removed {
                            transaction.commit()?;
                        }
                        Ok(removed)
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::FinalizeManifestVolumeRelease {
                workspace,
                id,
                name,
                generation,
                missing,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let removed = transaction.finalize_manifest_volume_release_candidate(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                            if missing {
                                "manifest.volume_release.reconciled_missing"
                            } else {
                                "manifest.volume_release.removed"
                            },
                        )?;
                        if removed {
                            transaction.commit()?;
                        }
                        Ok(removed)
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ConfirmSetupRetiredStopped {
                workspace,
                id,
                name,
                generation,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let recorded = transaction.confirm_setup_retired_container_stopped(
                            &workspace,
                            &id,
                            &name,
                            &generation,
                            now,
                        )?;
                        if recorded {
                            transaction.commit()?;
                        }
                        Ok(recorded)
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::CompleteSetupWorkspace { workspace, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = (|| {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| bosn_registry::Error::BadRow("system clock before epoch"))?
                            .as_secs_f64();
                        let mut transaction = registry.begin_immediate()?;
                        let completed = transaction.complete_setup_workspace(&workspace, now)?;
                        // An already-complete workspace is a genuine no-op:
                        // no event or timestamp write is committed.
                        if completed.uses_completed != 0 {
                            transaction.commit()?;
                        }
                        Ok(SetupDoneResult::from(completed))
                    })();
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::AppendSetupEnsureEvents { events, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = append_setup_ensure_events(&mut registry, &events);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::AppendManifestRecoveryEvents { events, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = append_manifest_recovery_events(&mut registry, &events);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::RecordSetupEnsure {
                job_id,
                execution,
                reply,
            } => {
                #[cfg(test)]
                if let Some(gate) = &mut setup_ensure_record_gate
                    && (gate.entered.send(()).await.is_err() || gate.release.recv().await.is_none())
                {
                    let _ = reply.send(Err(Error::ActorClosed));
                    continue;
                }
                let worker = async_engine::launch_blocking(move || {
                    let result = record_setup_ensure(&mut registry, job_id, &execution);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::RecordManifestEnsure {
                job_id,
                execution,
                contract,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        record_manifest_ensure(&mut registry, job_id, &execution, &contract);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestRecoveryContracts { reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry
                        .manifest_recovery_contract_details(MANIFEST_RECOVERY_MAX_CONTRACTS);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestAutostartIntentDisabled { detail, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.manifest_autostart_intent_disabled(&detail);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::ManifestRecoveryAuthorized { contract, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = registry.manifest_recovery_container_active(
                        &contract.resource_id,
                        &contract.name,
                        &contract.stack,
                        &contract.generation,
                        &contract.workspace,
                        &manifest_autostart_intent_detail(&contract),
                    );
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::PutManifestVolumeIntents { volumes, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = put_manifest_volume_intents(&mut registry, &volumes);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::RecordSetupAdoption { execution, reply } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = record_setup_adoption(&mut registry, &execution);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::BeginSetupAppTaskSession {
                job_id,
                container_id,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        record_setup_app_task_session(&mut registry, job_id, &container_id);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::FinishSetupAppTaskSession {
                job_id,
                outcome,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = finish_setup_app_task_session(&mut registry, job_id, outcome);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::BeginManifestAppTaskSession {
                job_id,
                container_id,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result =
                        record_manifest_app_task_session(&mut registry, job_id, &container_id);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::FinishManifestAppTaskSession {
                job_id,
                outcome,
                reply,
            } => {
                let worker = async_engine::launch_blocking(move || {
                    let result = finish_manifest_app_task_session(&mut registry, job_id, outcome);
                    (registry, result)
                });
                match worker.await {
                    Ok((returned, result)) => {
                        registry = returned;
                        let _ = reply.send(result.map_err(Error::Registry));
                    }
                    Err(_) => {
                        let _ = reply.send(Err(Error::ActorClosed));
                        return;
                    }
                }
            }
            DbCommand::Stop(reply) => {
                let _ = reply.send(());
                return;
            }
        }
    }
}
