//! Launch semantic jobs with bounded logs and durable completion.

use super::*;

#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
pub(crate) fn launch_started_setup_jobs(
    jobs: &mut Jobs,
    requests: &mut BTreeMap<u64, SetupJobRequest>,
    cancellations: &mut BTreeMap<u64, CancellationSource>,
    tasks: &mut async_engine::TaskGroup<()>,
    executors: &SetupExecutors,
    sender: async_engine::Sender<JobCommand>,
    registry: RegistryActor,
) {
    for id in jobs.take_started() {
        // Legacy generic jobs have no daemon executor.  Only the new semantic
        // operation may enter this branch, so no arbitrary command can escape
        // the typed setup boundary.
        let Some(mut request) = requests.remove(&id) else {
            continue;
        };
        let cancellation = CancellationSource::new();
        let token = cancellation.token();
        cancellations.insert(id, cancellation);
        let task_sender = sender.clone();
        let session_recorder = ActorSetupAppTaskSessionRecorder {
            actor: registry.clone(),
            job_id: id,
        };
        let prepare_executor = Arc::clone(&executors.prepare);
        let task_executor = Arc::clone(&executors.task);
        let app_task_executor = Arc::clone(&executors.app_task);
        let ensure_executor = Arc::clone(&executors.ensure);
        let manifest_ensure_executor = Arc::clone(&executors.manifest_ensure);
        let manifest_app_task_executor = Arc::clone(&executors.manifest_app_task);
        let run = match (&request, &executors.runners) {
            (SetupJobRequest::ManifestAppTask(request), Some(runners)) => {
                let slot = jobs.job(id).ok().and_then(|job| job.slot);
                let (record, activity) = runners.begin(
                    id,
                    &request.workspace.to_string_lossy(),
                    &request.stack,
                    &request.task_name,
                    slot,
                );
                Some(RunContext {
                    runners: Arc::clone(runners),
                    record,
                    activity,
                })
            }
            _ => None,
        };
        let manifest_session_recorder = ActorManifestAppTaskSessionRecorder {
            actor: registry.clone(),
            job_id: id,
            run,
        };
        let manifest_registry = registry.clone();
        let task_images = ActorSetupImageRecorder {
            actor: registry.clone(),
        };
        let state_dir = executors.state_dir.clone();
        tasks.spawn(async move {
            let admission_started = std::time::Instant::now();
            let _machine_admission =
                match managed_retention::gate::workload(*request.deadline_mut(), &token).await {
                    Ok(guard) => guard,
                    Err(error) => {
                        let _ = task_sender
                            .send(JobCommand::Completed {
                                id,
                                kind: request.kind(),
                                result: Err(error),
                            })
                            .await;
                        return;
                    }
                };
            let budget = request.deadline_mut();
            *budget = budget.saturating_sub(admission_started.elapsed());
            let (text_logs, mut log_receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
            let logs = if let Some(state_dir) = state_dir {
                let task = match &request {
                    SetupJobRequest::Prepare(_) => "setup-prepare",
                    SetupJobRequest::Task(request) => &request.task_name,
                    SetupJobRequest::AppTask(request) => &request.task_name,
                    SetupJobRequest::Ensure(_) => "setup-ensure",
                    SetupJobRequest::ManifestEnsure(_) => "manifest-ensure",
                    SetupJobRequest::ManifestConverge(_) => "manifest-converge",
                    SetupJobRequest::ManifestAppTask(request) => &request.task_name,
                };
                let raw = async {
                    let run_id = ci::wire::new_uuid().await.map_err(|e| e.message)?;
                    let raw = crate::raw_run_log::RawRunLog::create(&state_dir, &run_id)
                        .map_err(|e| e.to_string())?;
                    raw.write_metadata(&run_id, id, Some(task))
                        .map_err(|e| e.to_string())?;
                    Ok::<_, String>(raw)
                }
                .await;
                match raw {
                    Ok(raw) => {
                        let path = raw.root().display().to_string();
                        let sink = crate::raw_run_log::JobLogSink::durable(text_logs, raw);
                        let _ = sink.send(format!("[bosn] raw output: {path}")).await;
                        sink
                    }
                    Err(error) => {
                        let kind = request.kind();
                        let _ = task_sender
                            .send(JobCommand::Completed {
                                id,
                                kind,
                                result: Err(format!("could not create raw run log: {error}")),
                            })
                            .await;
                        return;
                    }
                }
            } else {
                crate::raw_run_log::JobLogSink::transient(text_logs)
            };
            let log_sender = task_sender.clone();
            let forwarder = async_engine::launch(async move {
                while let Some(line) = log_receiver.recv().await {
                    let mut lines = vec![line];
                    while lines.len() < LOG_BATCH_LINES {
                        match log_receiver.try_recv() {
                            Ok(line) => lines.push(line),
                            Err(_) => break,
                        }
                    }
                    if log_sender
                        .send(JobCommand::Log { id, lines })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
            let completion = match request {
                SetupJobRequest::Prepare(request) => {
                    let result = prepare_executor
                        .execute_recorded(request, &token, &logs, &task_images)
                        .await;
                    Some((SetupJobKind::Prepare, result))
                }
                SetupJobRequest::Task(request) => Some((
                    SetupJobKind::Task,
                    task_executor
                        .execute_recorded(request, &token, &logs, &task_images)
                        .await,
                )),
                SetupJobRequest::AppTask(request) => Some((
                    SetupJobKind::AppTask,
                    app_task_executor
                        .execute(request, &token, &logs, &session_recorder)
                        .await,
                )),
                SetupJobRequest::Ensure(request) => {
                    let result = ensure_executor
                        .execute_recorded(request, &token, &logs, &task_images)
                        .await;
                    match result {
                        Ok(execution) => {
                            let (reply, wait) = async_engine::oneshot_channel();
                            let persisted = if task_sender
                                .send(JobCommand::PersistSetupEnsure {
                                    id,
                                    execution,
                                    reply,
                                })
                                .await
                                .is_err()
                            {
                                Err("setup ensure registry actor stopped".to_owned())
                            } else {
                                match wait.await {
                                    Ok(result) => result,
                                    Err(_) => Err("setup ensure registry actor stopped".to_owned()),
                                }
                            };
                            persisted
                                .err()
                                .map(|error| (SetupJobKind::Ensure, Err(error)))
                        }
                        Err(error) => Some((SetupJobKind::Ensure, Err(error))),
                    }
                }
                SetupJobRequest::ManifestEnsure(request) => {
                    let result = manifest_ensure_executor
                        .execute(request.clone(), &token, &logs, &manifest_registry)
                        .await;
                    match result {
                        Ok(execution) => {
                            let contract = manifest_recovery_contract(&request, &execution, id);
                            let (reply, wait) = async_engine::oneshot_channel();
                            let persisted = match contract {
                                Err(error) => Err(error),
                                Ok(contract) => {
                                    if task_sender
                                        .send(JobCommand::PersistManifestEnsure {
                                            id,
                                            execution,
                                            contract,
                                            reply,
                                        })
                                        .await
                                        .is_err()
                                    {
                                        Err("manifest ensure registry actor stopped".to_owned())
                                    } else {
                                        match wait.await {
                                            Ok(result) => result,
                                            Err(_) => {
                                                Err("manifest ensure registry actor stopped"
                                                    .to_owned())
                                            }
                                        }
                                    }
                                }
                            };
                            persisted
                                .err()
                                .map(|error| (SetupJobKind::ManifestEnsure, Err(error)))
                        }
                        Err(error) => Some((SetupJobKind::ManifestEnsure, Err(error))),
                    }
                }
                SetupJobRequest::ManifestConverge(request) => Some((
                    SetupJobKind::ManifestConverge,
                    execute_manifest_converge(
                        id,
                        request,
                        manifest_ensure_executor.as_ref(),
                        &token,
                        &logs,
                        &manifest_registry,
                        &task_sender,
                    )
                    .await,
                )),
                SetupJobRequest::ManifestAppTask(request) => Some((
                    SetupJobKind::ManifestAppTask,
                    manifest_app_task_executor
                        .execute(request, &token, &logs, &manifest_session_recorder)
                        .await,
                )),
            };
            let state = if token.is_cancelled() {
                "cancelled"
            } else if completion
                .as_ref()
                .is_some_and(|(_, result)| result.is_err())
            {
                "failure"
            } else {
                "success"
            };
            if let Err(error) = logs.finish(state) {
                eprintln!("bosn: job {id} could not persist raw run completion: {error}");
            }
            drop(logs);
            let _ = forwarder.await;
            if let Some((kind, result)) = completion {
                let _ = task_sender
                    .send(JobCommand::Completed { id, kind, result })
                    .await;
            }
        });
    }
}

/// Execute the complete deterministic all-stack operation in the one daemon
/// job slot. Each member retains the existing manifest ensure executor and
/// its registry-backed volume intent/engine proof. A successful member is
/// recorded before the next starts, so later failure/cancellation never
/// erases a real earlier convergence.
pub(crate) async fn execute_manifest_converge(
    id: u64,
    request: ManifestConvergeJobRequest,
    executor: &dyn ManifestEnsureExecutor,
    cancellation: &async_engine::CancellationToken,
    logs: &crate::raw_run_log::JobLogSink,
    registry: &RegistryActor,
    job_sender: &async_engine::Sender<JobCommand>,
) -> Result<String, String> {
    let deadline = async_engine::Deadline::after(request.deadline);
    let stacks = async_engine::cancellable(
        cancellation,
        async_engine::timeout_at(deadline, async { manifest_converge_stack_names(&request) }),
    )
    .await
    .map_err(|_| "manifest converge cancelled before topology planning".to_owned())?
    .map_err(|_| "manifest converge planning exceeded its deadline".to_owned())??;
    let stack_count = stacks.len();
    let minimum = stack_count.saturating_mul(2);
    if request.output_limit < minimum {
        return Err("manifest converge output budget cannot fund every stack".into());
    }
    let output_per_stack = request.output_limit / stack_count;
    let output_remainder = request.output_limit % stack_count;
    for (index, stack) in stacks.iter().enumerate() {
        let remaining = deadline.remaining();
        if cancellation.is_cancelled() || remaining.is_zero() {
            return Err("manifest converge ended before the next stack".into());
        }
        let output_limit = output_per_stack + usize::from(index < output_remainder);
        logs.send(format!(
            "[manifest-converge] ensuring stack {stack} ({}/{stack_count})",
            index + 1
        ))
        .await
        .map_err(|_| "manifest converge log consumer closed".to_owned())?;
        let ensure_request = ManifestEnsureJobRequest {
            workspace: request.workspace.clone(),
            manifest: request.manifest.clone(),
            stack: stack.clone(),
            deadline: remaining,
            output_limit,
        };
        let execution = executor
            .execute(ensure_request.clone(), cancellation, logs, registry)
            .await
            .map_err(|error| format!("manifest converge stopped at stack {stack}: {error}"))?;
        let contract = manifest_recovery_contract(&ensure_request, &execution, id)
            .map_err(|error| format!("manifest converge stopped at stack {stack}: {error}"))?;
        let receipt = execution.receipt.clone();
        let (reply, wait) = async_engine::oneshot_channel();
        job_sender
            .send(JobCommand::PersistManifestConvergeStack {
                id,
                execution,
                contract,
                reply,
            })
            .await
            .map_err(|_| "manifest converge registry actor stopped".to_owned())?;
        wait.await
            .map_err(|_| "manifest converge registry actor stopped".to_owned())?
            .map_err(|error| format!("manifest converge stopped at stack {stack}: {error}"))?;
        logs.send(format!("[manifest-converge] {receipt}"))
            .await
            .map_err(|_| "manifest converge log consumer closed".to_owned())?;
    }
    Ok(format!("converged {stack_count} manifest stack(s)"))
}
