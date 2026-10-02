//! The job actor loop: admission, execution launch and completion of typed setup jobs.

use super::*;

pub(crate) async fn job_actor(
    mut jobs: Jobs,
    mut receiver: async_engine::Receiver<JobCommand>,
    executors: SetupExecutors,
    sender: async_engine::Sender<JobCommand>,
    registry: RegistryActor,
) {
    let mut requests: BTreeMap<u64, SetupJobRequest> = BTreeMap::new();
    // Job status is intentionally in-memory in this milestone. Keep just
    // enough typed identity to make its durable setup-ensure audit trail
    // complete without treating generic jobs as setup operations.
    let mut setup_kinds: BTreeMap<u64, SetupJobKind> = BTreeMap::new();
    let mut cancellations: BTreeMap<u64, CancellationSource> = BTreeMap::new();
    let mut tasks = async_engine::TaskGroup::new();
    let mut stopping: Option<async_engine::OneshotSender<()>> = None;
    let mut next_sweep = Instant::now();
    loop {
        // Wake at least once per sweep interval so a job whose follower
        // vanished is cancelled even when no other command arrives.
        let wait = next_sweep.saturating_duration_since(Instant::now());
        let command = match async_engine::timeout(wait, receiver.recv()).await {
            Ok(Some(command)) => Some(command),
            Ok(None) => break,
            Err(_) => None,
        };
        // Reaping finished tasks and the stall sweep run once per interval,
        // not once per command: a zero-length timeout still waits for a
        // timer tick (about a millisecond), which capped the whole daemon at
        // about a thousand commands a second and serialized every concurrent
        // job's log stream behind it (#358).
        let sweep = Instant::now() >= next_sweep;
        if sweep {
            next_sweep = Instant::now() + LEASE_SWEEP_INTERVAL;
            while matches!(
                async_engine::timeout(Duration::ZERO, tasks.join_next()).await,
                Ok(Some(_))
            ) {}
        }
        if sweep
            && stopping.is_none()
            && let Some(runners) = &executors.runners
            && let Some(after) = runners.capacity().stall_after
        {
            for id in jobs.stalled(Instant::now(), after, |id| runners.activity(id)) {
                let line = format!(
                    "[bosn] stalled: no output and no Docker activity for {}s; tearing down this job (stall_seconds)",
                    after.as_secs()
                );
                eprintln!("bosn: job {id} {}", &line[7..]);
                let _ = jobs.log(id, line);
                let _ = cancel_job_in_actor(
                    id,
                    &mut jobs,
                    &mut requests,
                    &mut setup_kinds,
                    &cancellations,
                    &registry,
                )
                .await;
            }
        }
        // Leases are checked on every command (a map scan, no timer), so a
        // lapsed follower's queued job is cancelled before a slot it was
        // waiting for is handed to it.
        if stopping.is_none() {
            for id in jobs.expired_leases(Instant::now()) {
                let _ = jobs.log(
                    id,
                    "[bosn] cancelling: the client following this job stopped polling (it exited or was killed)"
                        .into(),
                );
                let _ = cancel_job_in_actor(
                    id,
                    &mut jobs,
                    &mut requests,
                    &mut setup_kinds,
                    &cancellations,
                    &registry,
                )
                .await;
            }
        }
        let Some(command) = command else {
            if stopping.is_none() {
                // Cancelling a queued job can hand its slot to the next one.
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            } else if cancellations.is_empty() {
                while tasks.join_next().await.is_some() {}
                if let Some(reply) = stopping.take() {
                    let _ = reply.send(());
                }
                return;
            }
            continue;
        };
        match command {
            JobCommand::Submit {
                workspace,
                stack,
                digest,
                reply,
            } => {
                let result = jobs
                    .submit(&workspace, &stack, &digest)
                    .map(|s| match s {
                        Submission::Started(id)
                        | Submission::Queued(id)
                        | Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => replacement,
                    })
                    .map_err(|_| Error::Protocol("job admission"));
                let _ = reply.send(result);
            }
            JobCommand::Status { id, reply } => {
                jobs.touch(id, Instant::now());
                let _ = reply.send(jobs.job(id).map_err(|_| Error::Protocol("unknown job")));
            }
            JobCommand::Cancel { id, reply } => {
                let result = cancel_job_in_actor(
                    id,
                    &mut jobs,
                    &mut requests,
                    &mut setup_kinds,
                    &cancellations,
                    &registry,
                )
                .await;
                let _ = reply.send(result);
            }
            JobCommand::Logs {
                id,
                after,
                limit,
                reply,
            } => {
                jobs.touch(id, Instant::now());
                let _ = reply.send(
                    jobs.log_page(id, after, limit)
                        .map_err(|_| Error::Protocol("unknown job")),
                );
            }
            JobCommand::SubmitSetupPrepare { request, reply } => {
                let digest = setup_prepare_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit(&workspace, "setup-prepare", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::Prepare(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::Prepare(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitSetupTask { request, reply } => {
                let digest = setup_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit_class(&workspace, "setup-task", &digest, jobs::JobClass::Runner)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::Task(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::Task(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup task job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitSetupAppTask { request, reply } => {
                let digest = setup_app_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = jobs
                    .submit_class(
                        &workspace,
                        "setup-app-task",
                        &digest,
                        jobs::JobClass::Runner,
                    )
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::AppTask(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::AppTask(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("setup app task job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitSetupEnsure { request, reply } => {
                let digest = setup_ensure_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let result = match jobs.submit(&workspace, "setup-ensure", &digest) {
                    Ok(Submission::Joined(id)) => Ok(id),
                    Ok(Submission::Started(id)) | Ok(Submission::Queued(id)) => {
                        match registry
                            .append_setup_ensure_events(vec![SetupEnsureEvent::submitted(
                                id, &request,
                            )])
                            .await
                        {
                            Ok(()) => {
                                setup_kinds.insert(id, SetupJobKind::Ensure);
                                requests.insert(id, SetupJobRequest::Ensure(request));
                                Ok(id)
                            }
                            Err(error) => {
                                // Do not launch an operation whose durable
                                // submission audit could not be written.
                                let _ = jobs.settle_with_error(
                                    id,
                                    false,
                                    Some("setup ensure submission audit unavailable".into()),
                                );
                                Err(error)
                            }
                        }
                    }
                    Ok(Submission::Superseded { job, replacement }) => {
                        let mut events = Vec::new();
                        if setup_kinds.get(&job) == Some(&SetupJobKind::Ensure) {
                            events.push(SetupEnsureEvent::terminal(
                                job,
                                SetupEnsureEventOutcome::Superseded,
                            ));
                        }
                        events.push(SetupEnsureEvent::submitted(replacement, &request));
                        match registry.append_setup_ensure_events(events).await {
                            Ok(()) => {
                                setup_kinds.remove(&job);
                                requests.remove(&job);
                                setup_kinds.insert(replacement, SetupJobKind::Ensure);
                                requests.insert(replacement, SetupJobRequest::Ensure(request));
                                Ok(replacement)
                            }
                            Err(error) => {
                                let _ = jobs.settle_with_error(
                                    replacement,
                                    false,
                                    Some("setup ensure submission audit unavailable".into()),
                                );
                                requests.remove(&job);
                                setup_kinds.remove(&job);
                                Err(error)
                            }
                        }
                    }
                    Err(_) => Err(Error::Protocol("setup ensure job admission")),
                };
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitManifestEnsure { request, reply } => {
                let digest = manifest_ensure_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let job_stack = format!("manifest-ensure:{}", request.stack);
                let result = jobs
                    .submit(&workspace, &job_stack, &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::ManifestEnsure(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::ManifestEnsure(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("manifest ensure job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitManifestConverge { request, reply } => {
                let digest = manifest_converge_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                // Jobs are globally single-flight today. Giving a topology its
                // own coalescing key makes same-manifest submissions join while
                // preserving the existing per-stack API and ordering all
                // member volume/guest work through one parent job.
                let result = jobs
                    .submit(&workspace, "manifest-converge", &digest)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::ManifestConverge(request));
                            id
                        }
                        Submission::Joined(id) => id,
                        Submission::Superseded { replacement, .. } => {
                            requests
                                .insert(replacement, SetupJobRequest::ManifestConverge(request));
                            replacement
                        }
                    })
                    .map_err(|_| Error::Protocol("manifest converge job admission"));
                let _ = reply.send(result);
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::SubmitManifestAppTask {
                request,
                follow_lease,
                reply,
            } => {
                let digest = manifest_app_task_digest(&request);
                let workspace = request.workspace.to_string_lossy().into_owned();
                let job_stack = format!("manifest-app-task:{}", request.stack);
                let result = jobs
                    .submit_class(&workspace, &job_stack, &digest, jobs::JobClass::Runner)
                    .map(|submission| match submission {
                        Submission::Started(id) | Submission::Queued(id) => {
                            requests.insert(id, SetupJobRequest::ManifestAppTask(request));
                            (id, true)
                        }
                        Submission::Joined(id) => (id, false),
                        Submission::Superseded { replacement, .. } => {
                            requests.insert(replacement, SetupJobRequest::ManifestAppTask(request));
                            (replacement, true)
                        }
                    })
                    .map_err(|_| Error::Protocol("manifest app task job admission"));
                // Only the submission that created the job sets its lease: a
                // follower joining another caller's unleased job must not make
                // that job depend on this follower staying alive.
                if let (Ok((id, true)), Some(period)) = (&result, follow_lease) {
                    jobs.lease(*id, period, Instant::now());
                }
                let _ = reply.send(result.map(|(id, _)| id));
                launch_started_setup_jobs(
                    &mut jobs,
                    &mut requests,
                    &mut cancellations,
                    &mut tasks,
                    &executors,
                    sender.clone(),
                    registry.clone(),
                );
            }
            JobCommand::PersistSetupEnsure {
                id,
                execution,
                reply,
            } => {
                let result = if jobs
                    .job(id)
                    .is_ok_and(|job| job.state == jobs::JobState::Running)
                {
                    let receipt = execution.receipt.clone();
                    registry
                        .record_setup_ensure(id, execution)
                        .await
                        .map_err(|error| format!("setup ensure registry recording failed: {error}"))
                        .map(|()| {
                            // Settle before accepting another command. A
                            // cancellation processed before this command has
                            // already changed the state to Cancelling and is
                            // rejected above; a later cancellation observes a
                            // terminal success and cannot be accepted.
                            cancellations.remove(&id);
                            setup_kinds.remove(&id);
                            let _ = jobs.log(id, bounded_log_line(&receipt));
                            let _ = jobs.settle_with_error(id, true, None);
                        })
                } else {
                    Err("setup ensure cancelled".into())
                };
                let _ = reply.send(result);
            }
            JobCommand::PersistManifestEnsure {
                id,
                execution,
                contract,
                reply,
            } => {
                let result = if jobs
                    .job(id)
                    .is_ok_and(|job| job.state == jobs::JobState::Running)
                {
                    let receipt = execution.receipt.clone();
                    registry
                        .record_manifest_ensure(id, execution, contract)
                        .await
                        .map_err(|error| {
                            format!("manifest ensure registry recording failed: {error}")
                        })
                        .map(|()| {
                            cancellations.remove(&id);
                            let _ = jobs.log(id, bounded_log_line(&receipt));
                            let _ = jobs.settle_with_error(id, true, None);
                        })
                } else {
                    Err("manifest ensure cancelled".into())
                };
                let _ = reply.send(result);
            }
            JobCommand::PersistManifestConvergeStack {
                id,
                execution,
                contract,
                reply,
            } => {
                let result = if jobs
                    .job(id)
                    .is_ok_and(|job| job.state == jobs::JobState::Running)
                {
                    registry
                        .record_manifest_ensure(id, execution, contract)
                        .await
                        .map_err(|error| {
                            format!("manifest converge registry recording failed: {error}")
                        })
                } else {
                    Err("manifest converge cancelled".into())
                };
                let _ = reply.send(result);
            }
            JobCommand::Log { id, lines } => {
                // A full log record is never permitted to block daemon IPC;
                // bounded engine output instead applies back-pressure upstream.
                for line in lines {
                    let _ = jobs.log(id, bounded_log_line(&line));
                }
            }
            JobCommand::List { reply } => {
                let view = jobs_view(&jobs, executors.runners.as_deref());
                let _ = reply.send(view.to_string());
            }
            JobCommand::Completed { id, kind, result } => {
                cancellations.remove(&id);
                if let Some(runners) = &executors.runners {
                    runners.finish(id);
                }
                let terminal_outcome = if matches!(kind, SetupJobKind::Ensure) {
                    let cancelled = jobs
                        .job(id)
                        .is_ok_and(|job| job.state == jobs::JobState::Cancelling);
                    Some(if cancelled {
                        SetupEnsureEventOutcome::Cancelled
                    } else if result.is_ok() {
                        SetupEnsureEventOutcome::Succeeded
                    } else {
                        SetupEnsureEventOutcome::Failed
                    })
                } else {
                    None
                };
                if let Some(outcome) = terminal_outcome {
                    // A successful ensure settles in PersistSetupEnsure with
                    // its event and resources in one transaction. This branch
                    // therefore only records failed/cancelled terminal work.
                    if outcome != SetupEnsureEventOutcome::Succeeded
                        && setup_kinds.get(&id) == Some(&SetupJobKind::Ensure)
                    {
                        let _ = registry
                            .append_setup_ensure_events(vec![SetupEnsureEvent::terminal(
                                id, outcome,
                            )])
                            .await;
                    }
                }
                match result {
                    Ok(receipt) => {
                        let _ = jobs.log(id, bounded_log_line(&receipt));
                        let _ = jobs.settle_with_error(id, true, None);
                    }
                    Err(error) => {
                        let error = bounded_log_line(&error);
                        let operation = match kind {
                            SetupJobKind::Prepare => "setup prepare",
                            SetupJobKind::Task => "setup task",
                            SetupJobKind::AppTask => "setup app task",
                            SetupJobKind::Ensure => "setup ensure",
                            SetupJobKind::ManifestEnsure => "manifest ensure",
                            SetupJobKind::ManifestConverge => "manifest converge",
                            SetupJobKind::ManifestAppTask => "manifest app task",
                        };
                        let _ = jobs.log(id, format!("{operation} failed: {error}"));
                        let _ = jobs.settle_with_error(id, false, Some(error));
                    }
                }
                if jobs.job(id).is_ok_and(|job| job.state.terminal()) {
                    setup_kinds.remove(&id);
                }
            }
            JobCommand::Stop(reply) => {
                jobs.close();
                let cancelled: Vec<SetupEnsureEvent> = setup_kinds
                    .iter()
                    .filter_map(|(&id, kind)| {
                        (*kind == SetupJobKind::Ensure
                            && jobs
                                .job(id)
                                .is_ok_and(|job| job.state == jobs::JobState::Cancelled))
                        .then_some(SetupEnsureEvent::terminal(
                            id,
                            SetupEnsureEventOutcome::Cancelled,
                        ))
                    })
                    .collect();
                if !cancelled.is_empty() {
                    let _ = registry.append_setup_ensure_events(cancelled).await;
                }
                setup_kinds.retain(|&id, _| {
                    !jobs
                        .job(id)
                        .is_ok_and(|job| job.state == jobs::JobState::Cancelled)
                });
                for (&id, cancellation) in &cancellations {
                    // Preserve cancellation semantics in durable status while
                    // the executor owns direct-child reaping.
                    let _ = jobs.cancel(id);
                    cancellation.cancel();
                }
                stopping = Some(reply);
            }
        }
        // The completion path may have freed a slot. Launching only here
        // makes task ownership explicit and preserves the scheduler cap.
        if stopping.is_none() {
            launch_started_setup_jobs(
                &mut jobs,
                &mut requests,
                &mut cancellations,
                &mut tasks,
                &executors,
                sender.clone(),
                registry.clone(),
            );
        }
        if stopping.is_some() && cancellations.is_empty() {
            while tasks.join_next().await.is_some() {}
            if let Some(reply) = stopping.take() {
                let _ = reply.send(());
            }
            return;
        }
    }
}

/// How often the job actor checks follow leases when no command arrives.
const LEASE_SWEEP_INTERVAL: Duration = Duration::from_millis(500);
/// Most output lines one forwarded log command carries.
const LOG_BATCH_LINES: usize = 256;

/// Cancel one job: a queued job ends now, a running one is signalled and
/// settles when its executor completes. Shared by `bosn job cancel` and the
/// follow-lease sweep.
async fn cancel_job_in_actor(
    id: u64,
    jobs: &mut Jobs,
    requests: &mut BTreeMap<u64, SetupJobRequest>,
    setup_kinds: &mut BTreeMap<u64, SetupJobKind>,
    cancellations: &BTreeMap<u64, CancellationSource>,
    registry: &RegistryActor,
) -> Result<(), Error> {
    let result = jobs.cancel(id).map_err(|_| Error::Protocol("job cancel"));
    if result.is_ok() {
        if let Some(cancellation) = cancellations.get(&id) {
            cancellation.cancel();
        } else if jobs.job(id).is_ok_and(|job| job.state.terminal()) {
            requests.remove(&id);
            if setup_kinds.remove(&id) == Some(SetupJobKind::Ensure) {
                let _ = registry
                    .append_setup_ensure_events(vec![SetupEnsureEvent::terminal(
                        id,
                        SetupEnsureEventOutcome::Cancelled,
                    )])
                    .await;
            }
        }
    }
    result
}

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
        let Some(request) = requests.remove(&id) else {
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
        tasks.spawn(async move {
            let (logs, mut log_receiver) = async_engine::channel(SETUP_PREPARE_EVENT_QUEUE);
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
                SetupJobRequest::Prepare(request) => Some((
                    SetupJobKind::Prepare,
                    prepare_executor.execute(request, &token, &logs).await,
                )),
                SetupJobRequest::Task(request) => Some((
                    SetupJobKind::Task,
                    task_executor.execute(request, &token, &logs).await,
                )),
                SetupJobRequest::AppTask(request) => Some((
                    SetupJobKind::AppTask,
                    app_task_executor
                        .execute(request, &token, &logs, &session_recorder)
                        .await,
                )),
                SetupJobRequest::Ensure(request) => {
                    let result = ensure_executor.execute(request, &token, &logs).await;
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
    logs: &async_engine::Sender<String>,
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
