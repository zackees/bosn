//! One authenticated connection: decode a request, dispatch the typed operation, reply.

use super::*;
use bosn_core::retention::RetentionPolicy;

pub(crate) struct ConnectionContext {
    pub(crate) actor: RegistryActor,
    pub(crate) jobs: JobActor,
    pub(crate) stop: CancellationSource,
    pub(crate) doctor: Arc<dyn DoctorExecutor>,
    pub(crate) adopt: Arc<dyn SetupAdoptExecutor>,
    pub(crate) reconcile: Arc<dyn SetupReconcileExecutor>,
    pub(crate) state_dir: PathBuf,
    pub(crate) identity: Arc<DaemonIdentity>,
    pub(crate) ci: ci::CiRuntime,
}

/// How long stopping the daemon waits for its spare engine to be removed.
pub(crate) const SPARE_CLOSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

pub(crate) fn ci_error_wire(code: &str, message: String) -> ReplyWire {
    let reply = ci::ErrorReply {
        code: code.into(),
        message,
    };
    ReplyWire {
        code: 361,
        ci_reply: serde_json::to_string(&reply).unwrap_or_default(),
        ..Default::default()
    }
}

#[expect(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "baseline, ci.yml#229"
)]
pub(crate) async fn handle(mut s: AsyncStream, context: ConnectionContext) -> Result<(), Error> {
    let ConnectionContext {
        actor,
        jobs,
        stop,
        doctor,
        adopt,
        reconcile,
        state_dir,
        identity,
        ci,
    } = context;
    if !peer_is_authorized(&s.peer_identity()?.user_id, &ipc::current_user_id()?) {
        return Err(Error::Unauthorized);
    }
    let f = read_frame(&mut s).await?;
    if f.payload_protocol() != PAYLOAD_PROTOCOL
        || f.kind_classification() != DaemonFrameKind::Request
        || f.payload_encoding_classification() != DaemonPayloadEncoding::None
    {
        return Err(Error::Protocol("request frame"));
    }
    let r = Request::decode(f.payload()).map_err(|_| Error::Protocol("request decode"))?;
    let reply = if r.protocol_version != PROTOCOL_VERSION {
        ReplyWire {
            code: 1,
            ..Default::default()
        }
    } else {
        match r.operation {
            1 => ReplyWire {
                code: 10,
                daemon_version: identity.release.clone(),
                daemon_protocol: identity.protocol,
                ..Default::default()
            },
            2 => {
                let status = actor.status().await?;
                ReplyWire {
                    code: 20,
                    registry_id: status.registry_id,
                    schema_version: status.schema_version,
                    resources: status.resources,
                    leases: status.leases,
                    sessions: status.sessions,
                    reconciliation_required: status.reconciliation_required,
                    ..Default::default()
                }
            }
            3 => {
                // The spare engine (#410) goes before the daemon stops
                // answering, so `bosn daemon stop` returns with it removed.
                let _ = async_engine::timeout(SPARE_CLOSE_DEADLINE, ci.close_spares()).await;
                stop.cancel();
                ReplyWire {
                    code: 30,
                    ..Default::default()
                }
            }
            4 => match jobs.submit(r.workspace, r.stack, r.digest).await {
                Ok(job_id) => ReplyWire {
                    code: 40,
                    job_id,
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            38 => match validate_managed_retention_request_wire(&r) {
                Ok(()) => {
                    let policy = RetentionPolicy {
                        container_ttl: Duration::from_secs(r.owned_container_ttl_secs),
                        volume_ttl: Duration::from_secs(r.owned_volume_ttl_secs),
                        image_ttl: Duration::from_secs(r.owned_image_ttl_secs),
                        max_bytes: (r.owned_max_bytes > 0).then_some(i128::from(r.owned_max_bytes)),
                    };
                    let apply = r.owned_confirm;
                    let state_dir = state_dir.clone();
                    // The daemon re-derives the plan from its own fresh read here. A preview a
                    // client built earlier is never trusted: it was taken against engine state
                    // that may already have changed, and a preview is not an authorization.
                    let outcome = async_engine::launch_blocking(move || {
                        let engine = DockerEngine::docker();
                        managed_retention::managed_retention_pass(
                            &engine, &state_dir, policy, apply,
                        )
                    })
                    .await;
                    match outcome {
                        Ok(outcome) => {
                            let summary = outcome.summary;
                            ReplyWire {
                                code: 230,
                                owned_applied: summary.applied,
                                owned_planned: summary.planned,
                                owned_removed: summary.removed,
                                owned_removed_bytes: i64::try_from(summary.removed_bytes)
                                    .unwrap_or(i64::MAX),
                                owned_deferred: summary.deferred,
                                owned_failed: summary.failed,
                                owned_failures: summary.failures,
                                owned_refused: summary.refused.unwrap_or_default(),
                                ..Default::default()
                            }
                        }
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    }
                }
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            37 => match jobs.list().await {
                Ok(json) => ReplyWire {
                    code: 220,
                    jobs_json: json,
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            5 => match jobs.status(r.job_id).await {
                Ok(job) => ReplyWire {
                    code: 50,
                    job_id: job.id,
                    job_state: format!("{:?}", job.state),
                    job_error: job.error.unwrap_or_default(),
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            6 => match jobs.cancel(r.job_id).await {
                Ok(()) => ReplyWire {
                    code: 60,
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            7 => match jobs.logs(r.job_id, r.log_after, r.log_limit as usize).await {
                Ok(page) => ReplyWire {
                    code: 70,
                    retained_from: page.retained_from,
                    next_log_cursor: page.next,
                    log_gap: page.gap,
                    logs: page
                        .records
                        .into_iter()
                        .map(|(cursor, line)| LogRecordWire { cursor, line })
                        .collect(),
                    ..Default::default()
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            8 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_prepare_wire(
                        &r.workspace,
                        &r.setup_config,
                        policy,
                        r.setup_deadline_ms,
                        r.setup_output_limit,
                    )
                    .ok()
                    .map(|()| SetupPrepareRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_prepare(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            9 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_task_wire(
                        &r.workspace,
                        &r.setup_config,
                        policy,
                        &r.setup_task_name,
                        r.setup_deadline_ms,
                        r.setup_output_limit,
                    )
                    .ok()
                    .map(|()| SetupTaskJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        task_name: r.setup_task_name,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_task(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            10 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_ensure_request_wire(&r, policy)
                        .ok()
                        .map(|()| SetupEnsureJobRequest {
                            workspace: PathBuf::from(r.workspace),
                            config: r.setup_config,
                            policy,
                            deadline: Duration::from_millis(r.setup_deadline_ms),
                            output_limit: r.setup_output_limit as usize,
                        })
                });
                match request {
                    Some(request) => match jobs.submit_setup_ensure(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            21 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_task_wire(
                        &r.workspace,
                        &r.setup_config,
                        policy,
                        &r.setup_task_name,
                        r.setup_deadline_ms,
                        r.setup_output_limit,
                    )
                    .ok()
                    .map(|()| SetupAppTaskJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        config: r.setup_config,
                        policy,
                        task_name: r.setup_task_name,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                });
                match request {
                    Some(request) => match jobs.submit_setup_app_task(request).await {
                        Ok(job_id) => ReplyWire {
                            code: 40,
                            job_id,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    },
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            22 => match validate_manifest_ensure_request_wire(&r) {
                Ok(()) => match jobs
                    .submit_manifest_ensure(ManifestEnsureJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        manifest: r.setup_config,
                        stack: r.stack,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                    .await
                {
                    Ok(job_id) => ReplyWire {
                        code: 40,
                        job_id,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            24 => match validate_manifest_converge_request_wire(&r) {
                Ok(()) => match jobs
                    .submit_manifest_converge(ManifestConvergeJobRequest {
                        workspace: PathBuf::from(r.workspace),
                        manifest: r.setup_config,
                        deadline: Duration::from_millis(r.setup_deadline_ms),
                        output_limit: r.setup_output_limit as usize,
                    })
                    .await
                {
                    Ok(job_id) => ReplyWire {
                        code: 40,
                        job_id,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            23 => match validate_manifest_app_task_request_wire(&r) {
                Ok(()) => match jobs
                    .submit_manifest_app_task(
                        ManifestAppTaskJobRequest {
                            workspace: PathBuf::from(r.workspace),
                            manifest: r.setup_config,
                            stack: r.stack,
                            task_name: r.setup_task_name,
                            deadline: Duration::from_millis(r.setup_deadline_ms),
                            output_limit: r.setup_output_limit as usize,
                        },
                        (r.follow_lease_ms != 0).then(|| Duration::from_millis(r.follow_lease_ms)),
                    )
                    .await
                {
                    Ok(job_id) => ReplyWire {
                        code: 40,
                        job_id,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            11 => match validate_registry_diagnostics_request_wire(&r) {
                Ok(()) => match actor
                    .resources(r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 80,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        resources_diagnostic: page
                            .records
                            .into_iter()
                            .map(ResourceDiagnosticWire::from)
                            .collect(),
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            12 => match validate_registry_diagnostics_request_wire(&r) {
                Ok(()) => match actor
                    .setup_ensure_events(r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 90,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        setup_ensure_events: page
                            .records
                            .into_iter()
                            .map(SetupEnsureEventWire::from)
                            .collect(),
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            13 => {
                match validate_doctor_request_wire(&r) {
                    Ok(()) => {
                        let registry = actor.doctor_integrity().await;
                        let report =
                            match async_engine::timeout(DOCTOR_ENGINE_DEADLINE, doctor.doctor())
                                .await
                            {
                                Ok(report) => report,
                                Err(_) => DockerDoctorReport {
                                    state: DockerDoctorState::Deadline,
                                    client_version: None,
                                    server_version: None,
                                },
                            };
                        let report = DoctorReport::from_engine(registry, report);
                        ReplyWire {
                            code: 100,
                            doctor_daemon: report.daemon,
                            doctor_registry: report.registry,
                            doctor_engine: report.engine,
                            doctor_client_version: report.client_version.unwrap_or_default(),
                            doctor_server_version: report.server_version.unwrap_or_default(),
                            ..Default::default()
                        }
                    }
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            17 => {
                let policy = SetupPreparePolicy::from_wire(r.setup_policy);
                let request = policy.and_then(|policy| {
                    validate_setup_adopt_request_wire(&r, policy)
                        .ok()
                        .map(|()| SetupAdoptRequest {
                            workspace: PathBuf::from(r.workspace),
                            config: r.setup_config,
                            policy,
                            deadline: Duration::from_millis(r.setup_deadline_ms),
                            output_limit: r.setup_output_limit as usize,
                            confirm: true,
                        })
                });
                match request {
                    Some(request) => {
                        let (logs, mut receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
                        let logs = crate::raw_run_log::JobLogSink::transient(logs);
                        let drain = async_engine::launch(async move {
                            while receiver.recv().await.is_some() {}
                        });
                        let cancellation = async_engine::CancellationSource::new();
                        let token = cancellation.token();
                        let result = adopt.execute(request, &token, &logs).await;
                        drop(logs);
                        let _ = drain.await;
                        match result {
                            Ok(execution) => match actor.record_setup_adoption(execution).await {
                                Ok(()) => ReplyWire {
                                    code: 140,
                                    setup_adopted: true,
                                    ..Default::default()
                                },
                                Err(_) => ReplyWire {
                                    code: 3,
                                    ..Default::default()
                                },
                            },
                            Err(_) => ReplyWire {
                                code: 3,
                                ..Default::default()
                            },
                        }
                    }
                    None => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                }
            }
            14 => match validate_setup_gc_preview_request_wire(&r) {
                Ok(()) => match actor
                    .setup_gc_preview(r.workspace, r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 110,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        setup_gc_candidates: page
                            .candidates
                            .into_iter()
                            .map(SetupGcCandidateWire::from)
                            .collect(),
                        gc_protected_not_retired: page.counts.protected_not_retired,
                        gc_protected_ambiguous_use: page.counts.protected_ambiguous_use,
                        gc_protected_lease: page.counts.protected_lease,
                        gc_protected_session: page.counts.protected_session,
                        gc_excluded_unmanaged: page.counts.excluded_unmanaged,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            15 => match validate_setup_gc_apply_request_wire(&r) {
                Ok(()) => match apply_setup_gc_candidate(&actor, r.workspace, r.gc_candidate_token)
                    .await
                {
                    Ok(result) => ReplyWire {
                        code: 120,
                        gc_removed: result.removed,
                        gc_reconciled_missing: result.reconciled_missing,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            16 => match validate_setup_done_request_wire(&r)
                .and_then(|()| canonical_setup_done_workspace(Path::new(&r.workspace)))
            {
                Ok(workspace) => match actor.complete_setup_workspace(workspace).await {
                    Ok(result) => ReplyWire {
                        code: 130,
                        setup_done_uses: result.uses_completed,
                        setup_done_resources: result.resources_completed,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            18 => match validate_setup_retired_stop_request_wire(&r) {
                Ok(()) => {
                    match stop_setup_retired_candidate(&actor, r.workspace, r.gc_candidate_token)
                        .await
                    {
                        Ok(result) => ReplyWire {
                            code: 150,
                            setup_retired_stopped: result.stopped,
                            setup_retired_already_stopped: result.already_stopped,
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    }
                }
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            19 => match validate_setup_reconcile_preview_request_wire(&r) {
                Ok(()) => match actor
                    .setup_reconcile_preview(r.workspace, r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok((next, candidates)) => {
                        let mut records = Vec::with_capacity(candidates.len());
                        for candidate in candidates {
                            let inspected = reconcile.inspect(&candidate.resource.name).await;
                            let drift = classify_setup_reconcile(&candidate, inspected);
                            let repair_token = (drift == "missing" && candidate.missing_repairable)
                                .then(|| setup_reconcile_missing_token(&candidate));
                            records.push(SetupReconcileRecord {
                                id: candidate.resource.id,
                                name: candidate.resource.name,
                                generation: candidate.resource.generation,
                                repair_token,
                                drift: drift.into(),
                            });
                        }
                        ReplyWire {
                            code: 160,
                            diagnostic_next: next.unwrap_or(0),
                            diagnostic_has_next: next.is_some(),
                            setup_reconcile_records: records
                                .into_iter()
                                .map(SetupReconcileRecordWire::from)
                                .collect(),
                            ..Default::default()
                        }
                    }
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            20 => match validate_setup_reconcile_repair_missing_request_wire(&r) {
                Ok(()) => match repair_missing_setup_reconcile_candidate(
                    &actor,
                    reconcile.as_ref(),
                    r.workspace,
                    r.gc_candidate_token,
                )
                .await
                {
                    Ok(result) => ReplyWire {
                        code: 170,
                        setup_reconcile_repaired: result.repaired,
                        setup_reconcile_already_repaired: result.already_repaired,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            31 => match validate_manifest_volume_gc_preview_request_wire(&r) {
                Ok(()) => match actor
                    .manifest_volume_gc_preview(r.workspace, r.diagnostic_after, r.diagnostic_limit)
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 180,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        manifest_volume_gc_candidates: page
                            .candidates
                            .into_iter()
                            .map(ManifestVolumeGcCandidateWire::from)
                            .collect(),
                        volume_gc_protected_not_retired: page.counts.protected_not_retired,
                        volume_gc_protected_policy: page.counts.protected_policy,
                        volume_gc_protected_ambiguous_use: page.counts.protected_ambiguous_use,
                        volume_gc_protected_lease: page.counts.protected_lease,
                        volume_gc_protected_session: page.counts.protected_session,
                        volume_gc_protected_intent: page.counts.protected_intent,
                        volume_gc_excluded_unmanaged: page.counts.excluded_unmanaged,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            32 => match validate_manifest_volume_gc_apply_request_wire(&r) {
                Ok(()) => match apply_manifest_volume_gc_candidate(
                    &actor,
                    r.workspace,
                    r.gc_candidate_token,
                )
                .await
                {
                    Ok(result) => ReplyWire {
                        code: 190,
                        volume_gc_removed: result.removed,
                        volume_gc_reconciled_missing: result.reconciled_missing,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            33 => match validate_manifest_volume_gc_preview_request_wire(&r) {
                Ok(()) => match actor
                    .manifest_volume_release_preview(
                        r.workspace,
                        r.diagnostic_after,
                        r.diagnostic_limit,
                    )
                    .await
                {
                    Ok(page) => ReplyWire {
                        code: 180,
                        diagnostic_next: page.next.unwrap_or(0),
                        diagnostic_has_next: page.next.is_some(),
                        manifest_volume_gc_candidates: page
                            .candidates
                            .into_iter()
                            .map(ManifestVolumeGcCandidateWire::from)
                            .collect(),
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            34 => match validate_manifest_volume_release_apply_request_wire(&r) {
                Ok(()) => match apply_manifest_volume_release_candidate(
                    &actor,
                    r.workspace,
                    r.gc_candidate_token,
                )
                .await
                {
                    Ok(result) => ReplyWire {
                        code: 190,
                        volume_gc_removed: result.removed,
                        volume_gc_reconciled_missing: result.reconciled_missing,
                        ..Default::default()
                    },
                    Err(_) => ReplyWire {
                        code: 3,
                        ..Default::default()
                    },
                },
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            35 => match validate_unmanaged_apply_request_wire(&r) {
                Ok(()) => {
                    let include = r.unmanaged_include.clone();
                    let ttl_seconds = r.unmanaged_ttl_seconds;
                    // The daemon re-derives both the census and the plan here. A plan a
                    // client built earlier is never trusted: it was taken against state
                    // that may already have changed.
                    let outcome = async_engine::launch_blocking(move || {
                        let state_dir = state_dir.clone();
                        let our_registry = bosn_registry::Registry::open_read_only(
                            state_dir.join("registry.sqlite3"),
                        )
                        .ok()
                        .and_then(|registry| registry.registry_id().ok());
                        let config = bosn_core::CensusConfig {
                            ttl_seconds: if ttl_seconds == 0 {
                                bosn_core::DEFAULT_TTL_SECONDS
                            } else {
                                ttl_seconds as f64
                            },
                        };
                        let engine = DockerEngine::docker();
                        unmanaged::unmanaged_gc_apply(
                            &engine,
                            our_registry.as_deref(),
                            config,
                            &include,
                        )
                    })
                    .await;
                    match outcome {
                        Ok(outcome) => ReplyWire {
                            code: 200,
                            unmanaged_planned: outcome.plan.candidates.len() as u64,
                            unmanaged_removed: outcome.removed,
                            unmanaged_removed_bytes: i64::try_from(outcome.removed_bytes)
                                .unwrap_or(i64::MAX),
                            unmanaged_failed: outcome.failed,
                            unmanaged_failures: outcome.failures,
                            unmanaged_refused: outcome.refused.unwrap_or_default(),
                            ..Default::default()
                        },
                        Err(_) => ReplyWire {
                            code: 3,
                            ..Default::default()
                        },
                    }
                }
                Err(_) => ReplyWire {
                    code: 3,
                    ..Default::default()
                },
            },
            36 => match serde_json::from_str::<ci::CiRequest>(&r.ci_request) {
                Err(error) => ci_error_wire("invalid_request", error.to_string()),
                Ok(request) => match ci.handle(request).await {
                    Ok(value) => ReplyWire {
                        code: 360,
                        ci_reply: value.to_string(),
                        ..Default::default()
                    },
                    Err(error) => ci_error_wire(error.code, error.message),
                },
            },
            _ => ReplyWire {
                code: 2,
                ..Default::default()
            },
        }
    };
    let mut p = Vec::new();
    reply
        .encode(&mut p)
        .map_err(|_| Error::Protocol("reply encode"))?;
    write_frame(&mut s, DaemonFrame::response_to(&f, p)).await
}
