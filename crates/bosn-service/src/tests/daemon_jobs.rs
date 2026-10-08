//! Typed jobs through a live daemon: prepare, task and app-task coalescing and sessions.

use super::*;

#[test]
fn fresh_daemon_serves_typed_client_and_releases_writer() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let first = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        client.ping().await.unwrap();
        let before = client.status().await.unwrap();
        assert_eq!(before.schema_version, 5);
        assert!(!before.registry_id.is_empty());
        client.shutdown().await.unwrap();
        stopped(first).await;
        let second = async_engine::launch(Service::new(state.clone()).serve());
        let after = wait_for_client(&state).await.status().await.unwrap();
        assert_eq!(after.registry_id, before.registry_id);
        wait_for_client(&state).await.shutdown().await.unwrap();
        stopped(second).await;
    });
}

#[test]
fn typed_job_submission_is_authenticated_and_coalesces() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let client = wait_for_client(&state).await;
            let one = client
                .submit_job("workspace", "stack", "sha256:abc")
                .await
                .unwrap();
            let two = client
                .submit_job("workspace", "stack", "sha256:abc")
                .await
                .unwrap();
            assert_eq!(one, two);
            let status = client.job_status(one).await.unwrap();
            assert_eq!(status.id, one);
            assert_eq!(status.state, "Running");
            assert_eq!(
                client.job_logs(one, 0, 16).await.unwrap(),
                JobLogPage {
                    retained_from: 0,
                    next: 0,
                    gap: false,
                    records: Vec::new(),
                }
            );
            client.cancel_job(one).await.unwrap();
            assert_eq!(client.job_status(one).await.unwrap().state, "Cancelling");
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn setup_prepare_job_is_prompt_coalesced_logged_and_cancellable_without_docker() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(SlowFakeSetupExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_prepare_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = SetupPrepareRequest {
                workspace: workspace.clone(),
                config: "https://example.invalid/setup.toml".into(),
                policy: SetupPreparePolicy::Refresh,
                deadline: Duration::from_secs(2),
                output_limit: 4 * 1024,
            };
            let submitted = std::time::Instant::now();
            let first = client.submit_setup_prepare(request.clone()).await.unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(
                first,
                client.submit_setup_prepare(request.clone()).await.unwrap()
            );
            wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
            // Slow execution does not occupy the daemon request actor.
            client.ping().await.unwrap();
            let logs = wait_for_logs(&client, first).await;
            assert!(logs.records[0].line.starts_with("[bosn] raw output: "));
            let mut fake_logged = false;
            for _ in 0..100 {
                let page = client.job_logs(first, 0, 16).await.unwrap();
                if page
                    .records
                    .iter()
                    .any(|record| record.line == "[fake] preparation started")
                {
                    fake_logged = true;
                    break;
                }
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            assert!(fake_logged, "the executor's output reaches the job log");

            let changed = SetupPrepareRequest {
                config: "https://example.invalid/other.toml".into(),
                ..request
            };
            let second = client.submit_setup_prepare(changed).await.unwrap();
            assert_ne!(first, second);
            client.cancel_job(first).await.unwrap();
            wait_for_job_state(&client, first, "Cancelled").await;
            wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
            client.cancel_job(second).await.unwrap();
            wait_for_job_state(&client, second, "Cancelled").await;
            assert_eq!(fake.cancelled.load(Ordering::SeqCst), 2);
            let runs = crate::raw_run_log::list_runs(&state).unwrap();
            for job_id in [first, second] {
                let run = runs.iter().find(|run| run.job_id == job_id).unwrap();
                assert_eq!(run.task.as_deref(), Some("setup-prepare"));
                assert_eq!(run.state.as_deref(), Some("cancelled"));
                assert!(run.ended_unix_ms.is_some());
                assert!(
                    crate::raw_run_log::read_end(&state, &run.run_id)
                        .unwrap()
                        .unwrap()
                        .exit_code
                        .is_none()
                );
            }
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn setup_prepare_coalescing_digest_covers_every_immutable_input() {
    let base = SetupPrepareRequest {
        workspace: PathBuf::from("/workspace"),
        config: "https://example.invalid/setup.toml".into(),
        policy: SetupPreparePolicy::Refresh,
        deadline: Duration::from_secs(2),
        output_limit: 4 * 1024,
    };
    let variants = [
        SetupPrepareRequest {
            workspace: PathBuf::from("/other"),
            ..base.clone()
        },
        SetupPrepareRequest {
            config: "https://example.invalid/other.toml".into(),
            ..base.clone()
        },
        SetupPrepareRequest {
            policy: SetupPreparePolicy::Offline,
            ..base.clone()
        },
        SetupPrepareRequest {
            deadline: Duration::from_secs(3),
            ..base.clone()
        },
        SetupPrepareRequest {
            output_limit: 8 * 1024,
            ..base.clone()
        },
    ];
    for variant in variants {
        assert_ne!(setup_prepare_digest(&base), setup_prepare_digest(&variant));
    }
}

#[test]
fn setup_task_job_is_prompt_coalesced_bounded_and_cancellable_without_docker() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(FakeSetupTaskExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_task_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = SetupTaskJobRequest {
                workspace: workspace.clone(),
                config: "https://example.invalid/setup.toml".into(),
                policy: SetupPreparePolicy::Refresh,
                task_name: "wait".into(),
                deadline: Duration::from_secs(2),
                output_limit: 4 * 1024,
            };
            let submitted = std::time::Instant::now();
            let first = client.submit_setup_task(request.clone()).await.unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(
                first,
                client.submit_setup_task(request.clone()).await.unwrap()
            );
            wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
            client.ping().await.unwrap();
            let logs = wait_for_logs(&client, first).await;
            assert!(
                logs.records
                    .iter()
                    .all(|record| record.line.len() <= jobs::MAX_LOG_LINE_BYTES)
            );

            let changed = SetupTaskJobRequest {
                task_name: "build".into(),
                ..request
            };
            let second = client.submit_setup_task(changed).await.unwrap();
            assert_ne!(first, second);
            client.cancel_job(first).await.unwrap();
            wait_for_job_state(&client, first, "Cancelled").await;
            wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
            wait_for_job_state(&client, second, "Succeeded").await;
            let completed_logs = client.job_logs(second, 0, 16).await.unwrap();
            assert!(
                completed_logs
                    .records
                    .iter()
                    .any(|record| record.line.len() == jobs::MAX_LOG_LINE_BYTES)
            );
            assert_eq!(fake.cancelled.load(Ordering::SeqCst), 1);
            assert_eq!(
                fake.stages(),
                vec![
                    "plan:wait",
                    "prepare:wait",
                    "plan:build",
                    "prepare:build",
                    "task:build",
                ]
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn setup_app_task_is_prompt_typed_and_clears_its_durable_session() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (gate, mut control) = execution_gate();
    let fake = Arc::new(FakeSetupAppTaskExecutor::new(Some(gate)));
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_app_task_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = SetupAppTaskJobRequest {
                workspace,
                config: "https://example.invalid/setup.toml".into(),
                policy: SetupPreparePolicy::Refresh,
                task_name: "check".into(),
                deadline: Duration::from_secs(2),
                output_limit: 4096,
            };
            let submitted = std::time::Instant::now();
            let first = client.submit_setup_app_task(request.clone()).await.unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            // Coalescing covers active jobs (#299): hold the executor until
            // the second IPC request has joined the first one.
            control.entered().await;
            assert_eq!(
                first,
                client.submit_setup_app_task(request).await.unwrap(),
                "identical semantic app-task requests coalesce"
            );
            control.release().await;
            wait_for_job_state(&client, first, "Succeeded").await;
            assert_eq!(fake.started.load(Ordering::SeqCst), 1);
            assert_eq!(*fake.observed.lock().unwrap(), vec!["check"]);
            assert_eq!(client.status().await.unwrap().sessions, 0);
            let logs = client.job_logs(first, 0, 8).await.unwrap();
            assert!(
                logs.records
                    .iter()
                    .any(|record| record.line.contains("declared app task"))
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn manifest_app_task_is_prompt_typed_and_clears_its_durable_session() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (gate, mut control) = execution_gate();
    let fake = Arc::new(FakeManifestAppTaskExecutor::new(gate));
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_app_task_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = ManifestAppTaskJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                task_name: "check".into(),
                deadline: Duration::from_secs(2),
                output_limit: 4096,
            };
            let first = client
                .submit_manifest_app_task(request.clone())
                .await
                .unwrap();
            // Coalescing covers active jobs: keep this executor active
            // until the second IPC request has joined the first one.
            control.entered().await;
            assert_eq!(
                first,
                client.submit_manifest_app_task(request).await.unwrap()
            );
            control.release().await;
            wait_for_job_state(&client, first, "Succeeded").await;
            assert_eq!(fake.observed.lock().unwrap().len(), 1);
            assert_eq!(client.status().await.unwrap().sessions, 0);
            assert!(
                client
                    .job_logs(first, 0, 8)
                    .await
                    .unwrap()
                    .records
                    .iter()
                    .any(|record| record.line.contains("manifest app task"))
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

/// Runs until its job is cancelled, like an `act` task would.
struct CancellableManifestAppTaskExecutor {
    started: async_engine::Sender<String>,
}
impl ManifestAppTaskExecutor for CancellableManifestAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        _logs: &'a crate::raw_run_log::JobLogSink,
        _session: &'a dyn ManifestAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self.started.send(request.task_name).await;
            cancellation.cancelled().await;
            Err("cancelled".into())
        })
    }
}

async fn next_task_start(wait: &mut async_engine::Receiver<String>) -> String {
    async_engine::timeout(Duration::from_secs(5), wait.recv())
        .await
        .unwrap()
        .unwrap()
}

/// #357: a `bosn run` client killed by SIGTERM/SIGHUP/SIGKILL stops
/// polling. Its job, running or still queued, must be cancelled rather
/// than run on to its deadline; a job without a lease, or one whose
/// follower keeps polling, must not be.
#[test]
fn a_followed_app_task_is_cancelled_once_its_follower_stops_polling() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let (started, mut started_wait) = async_engine::channel(8);
    let fake = Arc::new(CancellableManifestAppTaskExecutor { started });
    let request = |task: &str| ManifestAppTaskJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        task_name: task.into(),
        deadline: Duration::from_secs(600),
        output_limit: 4096,
    };
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_app_task_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let lease = Duration::from_secs(1);
            assert!(
                client
                    .follow_manifest_app_task(request("bad"), Duration::from_millis(999))
                    .await
                    .is_err(),
                "a lease below the minimum is refused"
            );

            // A follower that goes silent, with a second job of its
            // queued behind the first.
            let running = client
                .follow_manifest_app_task(request("running"), lease)
                .await
                .unwrap();
            assert_eq!(next_task_start(&mut started_wait).await, "running");
            let queued = client
                .follow_manifest_app_task(request("queued"), lease)
                .await
                .unwrap();
            async_engine::sleep(Duration::from_secs(3)).await;
            assert_eq!(client.job_status(running).await.unwrap().state, "Cancelled");
            assert_eq!(client.job_status(queued).await.unwrap().state, "Cancelled");
            assert!(
                client
                    .job_logs(running, 0, 16)
                    .await
                    .unwrap()
                    .records
                    .iter()
                    .any(|record| record.line.contains("stopped polling")),
                "the job log says why it was cancelled"
            );

            // Without a lease, silence changes nothing (#12: the job
            // outlives its CLI unless that CLI asked otherwise).
            let unleased = client
                .submit_manifest_app_task(request("unleased"))
                .await
                .unwrap();
            assert_eq!(next_task_start(&mut started_wait).await, "unleased");
            async_engine::sleep(Duration::from_secs(2)).await;
            assert_eq!(client.job_status(unleased).await.unwrap().state, "Running");
            client.cancel_job(unleased).await.unwrap();
            wait_for_job_state(&client, unleased, "Cancelled").await;

            // A follower that keeps polling keeps its job.
            let followed = client
                .follow_manifest_app_task(request("followed"), lease)
                .await
                .unwrap();
            assert_eq!(next_task_start(&mut started_wait).await, "followed");
            for _ in 0..12 {
                assert_eq!(client.job_status(followed).await.unwrap().state, "Running");
                async_engine::sleep(Duration::from_millis(200)).await;
            }
            async_engine::sleep(Duration::from_secs(3)).await;
            assert_eq!(
                client.job_status(followed).await.unwrap().state,
                "Cancelled"
            );

            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn uncertain_app_task_uses_verified_managed_receipt_identity_to_protect_gc() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let database = state.join("registry.sqlite3");
    let mut registry =
        Registry::create_writer(&database, "11111111-2222-4333-8444-555555555555").unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    let generation = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let resource_id = format!("setup-container:{generation}");
    let container_name = format!("bosn-setup-{generation}");
    transaction
        .put_resource(&Resource {
            id: resource_id.clone(),
            kind: ResourceKind::Container,
            name: container_name.clone(),
            stack: "setup".into(),
            generation: format!("sha256:{generation}"),
            scope: Scope::Machine,
            workspace: "/workspace".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Retired,
            retention: Retention::Pinned,
        })
        .unwrap();
    transaction
        .put_resource_use(&ResourceUse {
            resource_id: resource_id.clone(),
            workspace: "/workspace".into(),
            stack: "setup".into(),
            generation: format!("sha256:{generation}"),
            last_used: 1.0,
            state: ResourceState::Retired,
        })
        .unwrap();
    transaction.commit().unwrap();

    // This has the actual receipt shape returned by the ownership-safe
    // Docker inspection: its opaque Docker ID must never be used as the
    // durable registry/GC key.
    let observed = SetupEnsureResult {
        container_name: container_name.clone(),
        container_id: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
        image_identity: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        created: false,
        started: false,
        running: true,
    };
    let managed_identity = setup_app_task_session_container_identity(&observed);
    assert_eq!(managed_identity, container_name);
    assert_ne!(managed_identity, observed.container_id);
    record_setup_app_task_session(&mut registry, 7, &managed_identity).unwrap();
    finish_setup_app_task_session(&mut registry, 7, "uncertain").unwrap();
    assert_eq!(registry.resources(0, 16).unwrap().items[0].last_used, 1.0);
    assert_eq!(registry.status().unwrap().sessions, 1);
    assert_eq!(
        registry.execution_sessions(0, 1).unwrap().items[0].container_id,
        container_name
    );
    let protected = registry.setup_gc_preview("/workspace", 0, 16).unwrap();
    assert!(protected.candidates.items.is_empty());
    assert_eq!(protected.counts.protected_session, 1);

    finish_setup_app_task_session(&mut registry, 7, "failed").unwrap();
    assert!(registry.resources(0, 16).unwrap().items[0].last_used > 1.0);
    assert_eq!(registry.status().unwrap().sessions, 0);
    let eligible = registry.setup_gc_preview("/workspace", 0, 16).unwrap();
    assert_eq!(eligible.candidates.items.len(), 1);
    assert_eq!(eligible.candidates.items[0].id, resource_id);
}

#[test]
fn uncertain_manifest_app_task_session_protects_matching_manifest_container() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let database = temporary.path().join("registry.sqlite3");
    let mut registry =
        Registry::create_writer(&database, "11111111-2222-4333-8444-555555555555").unwrap();
    let generation = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let name = "bosn-setup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let id =
        "manifest-container:app:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&Resource {
            id: id.into(),
            kind: ResourceKind::Container,
            name: name.into(),
            stack: "app".into(),
            generation: generation.into(),
            scope: Scope::Machine,
            workspace: "/workspace".into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Retired,
            retention: Retention::Pinned,
        })
        .unwrap();
    transaction
        .put_resource_use(&ResourceUse {
            resource_id: id.into(),
            workspace: "/workspace".into(),
            stack: "app".into(),
            generation: generation.into(),
            last_used: 1.0,
            state: ResourceState::Retired,
        })
        .unwrap();
    transaction.commit().unwrap();
    record_manifest_app_task_session(&mut registry, 8, name).unwrap();
    finish_manifest_app_task_session(&mut registry, 8, "uncertain").unwrap();
    assert_eq!(registry.resources(0, 16).unwrap().items[0].last_used, 1.0);
    assert_eq!(
        registry.execution_sessions(0, 1).unwrap().items[0].container_id,
        name
    );
    assert!(
        registry
            .setup_gc_preview("/workspace", 0, 16)
            .unwrap()
            .candidates
            .items
            .is_empty()
    );
    finish_manifest_app_task_session(&mut registry, 8, "failed").unwrap();
    assert!(registry.resources(0, 16).unwrap().items[0].last_used > 1.0);
    assert_eq!(
        registry
            .setup_gc_preview("/workspace", 0, 16)
            .unwrap()
            .candidates
            .items
            .len(),
        1
    );
}

#[test]
fn setup_task_stops_after_prepare_failure_before_running_task() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(FakeSetupTaskExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_task_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let job = client
                .submit_setup_task(SetupTaskJobRequest {
                    workspace,
                    config: "https://example.invalid/setup.toml".into(),
                    policy: SetupPreparePolicy::Offline,
                    task_name: "prepare-fail".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                })
                .await
                .unwrap();
            wait_for_job_state(&client, job, "Failed").await;
            let logs = wait_for_logs(&client, job).await;
            assert!(
                logs.records
                    .iter()
                    .any(|record| record.line.contains("setup task failed"))
            );
            assert_eq!(
                fake.stages(),
                vec!["plan:prepare-fail", "prepare:prepare-fail"]
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn setup_task_coalescing_digest_covers_every_immutable_input() {
    let base = SetupTaskJobRequest {
        workspace: PathBuf::from("/workspace"),
        config: "https://example.invalid/setup.toml".into(),
        policy: SetupPreparePolicy::Refresh,
        task_name: "build".into(),
        deadline: Duration::from_secs(2),
        output_limit: 4 * 1024,
    };
    let variants = [
        SetupTaskJobRequest {
            workspace: PathBuf::from("/other"),
            ..base.clone()
        },
        SetupTaskJobRequest {
            config: "https://example.invalid/other.toml".into(),
            ..base.clone()
        },
        SetupTaskJobRequest {
            policy: SetupPreparePolicy::Offline,
            ..base.clone()
        },
        SetupTaskJobRequest {
            task_name: "test".into(),
            ..base.clone()
        },
        SetupTaskJobRequest {
            deadline: Duration::from_secs(3),
            ..base.clone()
        },
        SetupTaskJobRequest {
            output_limit: 8 * 1024,
            ..base.clone()
        },
    ];
    for variant in variants {
        assert_ne!(setup_task_digest(&base), setup_task_digest(&variant));
    }
}

#[test]
fn setup_task_wire_validation_bounds_and_rejects_nonsemantic_names() {
    let valid = || {
        validate_setup_task_wire(
            "/workspace",
            "https://example.invalid/setup.toml",
            SetupPreparePolicy::Refresh,
            "build-1",
            1,
            1,
        )
    };
    assert!(valid().is_ok());
    for task_name in ["", "-build", "build/task", &"a".repeat(65)] {
        assert!(
            validate_setup_task_wire(
                "/workspace",
                "https://example.invalid/setup.toml",
                SetupPreparePolicy::Offline,
                task_name,
                1,
                1,
            )
            .is_err()
        );
    }
    for (deadline, output) in [(0, 1), (300_001, 1), (1, 0), (1, 8_388_609)] {
        assert!(
            validate_setup_task_wire(
                "/workspace",
                "https://example.invalid/setup.toml",
                SetupPreparePolicy::Refresh,
                "build",
                deadline,
                output,
            )
            .is_err()
        );
    }
}

/// Runs until cancelled; with `tick`, logs a line every `tick` (#358).
struct TickingManifestAppTaskExecutor {
    started: async_engine::Sender<String>,
    tick: Option<Duration>,
}
impl ManifestAppTaskExecutor for TickingManifestAppTaskExecutor {
    fn execute<'a>(
        &'a self,
        request: ManifestAppTaskJobRequest,
        cancellation: &'a async_engine::CancellationToken,
        logs: &'a crate::raw_run_log::JobLogSink,
        _session: &'a dyn ManifestAppTaskSessionRecorder,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let _ = self
                .started
                .send(request.workspace.to_string_lossy().into_owned())
                .await;
            loop {
                let wait = self.tick.unwrap_or(Duration::from_secs(3600));
                if async_engine::cancellable(cancellation, async_engine::sleep(wait))
                    .await
                    .is_err()
                {
                    return Err("cancelled".into());
                }
                let _ = logs.send("tick".into()).await;
            }
        })
    }
}

fn test_capacity(slots: usize, stall: Option<Duration>) -> capacity::RunnerCapacity {
    capacity::RunnerCapacity {
        runner_slots: slots,
        control_slots: 2,
        cpus_per_slot: 4.0,
        memory_per_slot: None,
        stall_after: stall,
        docker_proxy: false,
    }
}

fn app_task(workspace: &str) -> ManifestAppTaskJobRequest {
    ManifestAppTaskJobRequest {
        workspace: PathBuf::from(workspace),
        manifest: "bosn.toml".into(),
        stack: "act".into(),
        task_name: "act-ci".into(),
        deadline: Duration::from_secs(600),
        output_limit: 4096,
    }
}

/// #358 RED -> GREEN: under `Jobs::new(1)` workspace B's task stayed
/// Queued while A's ran. Distinct workspaces now run in parallel up to
/// the runner slots; beyond them a job queues, and starts when a slot
/// frees. `bosn jobs` accounts for all of it.
#[test]
fn app_tasks_from_distinct_workspaces_run_in_parallel_up_to_the_slots() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let (started, mut started_wait) = async_engine::channel(8);
    let fake = Arc::new(TickingManifestAppTaskExecutor {
        started,
        tick: None,
    });
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_app_task_executor(fake.clone())
                    .with_runner_capacity(test_capacity(2, None))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let a = client
                .submit_manifest_app_task(app_task("/w/a"))
                .await
                .unwrap();
            let b = client
                .submit_manifest_app_task(app_task("/w/b"))
                .await
                .unwrap();
            let mut seen = vec![
                next_task_start(&mut started_wait).await,
                next_task_start(&mut started_wait).await,
            ];
            seen.sort();
            assert_eq!(seen, vec!["/w/a", "/w/b"], "B starts while A runs");
            let c = client
                .submit_manifest_app_task(app_task("/w/c"))
                .await
                .unwrap();
            async_engine::sleep(Duration::from_millis(300)).await;
            assert_eq!(client.job_status(c).await.unwrap().state, "Queued");

            let view = client.jobs().await.unwrap();
            assert_eq!(view["capacity"]["runner_slots"], 2);
            assert_eq!(view["capacity"]["cpus_per_slot"], 4.0);
            assert_eq!(view["lanes"]["runner"]["running"], 2);
            assert_eq!(view["lanes"]["runner"]["queued"], 1);
            let jobs = view["jobs"].as_array().unwrap();
            let entry = |id: u64| jobs.iter().find(|j| j["id"] == id).unwrap().clone();
            let mut slots = vec![entry(a)["slot"].as_u64(), entry(b)["slot"].as_u64()];
            slots.sort();
            assert_eq!(slots, vec![Some(0), Some(1)]);
            assert_eq!(entry(c)["state"], "queued");
            assert_eq!(entry(a)["class"], "runner");
            assert_eq!(entry(a)["run"]["task"], "act-ci", "accounting record");
            assert_eq!(entry(a)["run"]["nano_cpus"], 4_000_000_000_i64);

            // Freeing a slot admits the queued job into it.
            client.cancel_job(a).await.unwrap();
            wait_for_job_state(&client, a, "Cancelled").await;
            assert_eq!(next_task_start(&mut started_wait).await, "/w/c");
            let view = client.jobs().await.unwrap();
            let jobs = view["jobs"].as_array().unwrap();
            let c_entry = jobs.iter().find(|j| j["id"] == c).unwrap();
            assert_eq!(c_entry["slot"], entry(a)["slot"]);
            assert!(
                state.join("runners/active.json").exists(),
                "running tasks are in the ledger"
            );
            for id in [b, c] {
                client.cancel_job(id).await.unwrap();
                wait_for_job_state(&client, id, "Cancelled").await;
            }
            let ledger = std::fs::read_to_string(state.join("runners/active.json")).unwrap();
            assert_eq!(ledger.trim(), "[]", "finished runs leave the ledger");
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

/// A running task with no output (and no Docker activity) for longer
/// than the stall timeout is torn down; a chatty one is left alone.
#[test]
fn a_silent_task_is_torn_down_as_stalled_and_a_chatty_one_is_not() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let (started, mut started_wait) = async_engine::channel(8);
    let silent = Arc::new(TickingManifestAppTaskExecutor {
        started: started.clone(),
        tick: None,
    });
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_app_task_executor(silent.clone())
                    .with_runner_capacity(test_capacity(4, Some(Duration::from_secs(1))))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let id = client
                .submit_manifest_app_task(app_task("/w/quiet"))
                .await
                .unwrap();
            next_task_start(&mut started_wait).await;
            let deadline = Instant::now() + Duration::from_secs(10);
            while client.job_status(id).await.unwrap().state != "Cancelled" {
                assert!(Instant::now() < deadline, "a silent task is torn down");
                async_engine::sleep(Duration::from_millis(50)).await;
            }
            let page = client.job_logs(id, 0, 64).await.unwrap();
            assert!(
                page.records.iter().any(|r| r.line.contains("stalled")),
                "the job log says why it was torn down"
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    let chatty = Arc::new(TickingManifestAppTaskExecutor {
        started,
        tick: Some(Duration::from_millis(200)),
    });
    let state = temporary.path().join("state2");
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_app_task_executor(chatty.clone())
                    .with_runner_capacity(test_capacity(4, Some(Duration::from_secs(1))))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let id = client
                .submit_manifest_app_task(app_task("/w/chatty"))
                .await
                .unwrap();
            next_task_start(&mut started_wait).await;
            async_engine::sleep(Duration::from_secs(3)).await;
            assert_eq!(client.job_status(id).await.unwrap().state, "Running");
            client.cancel_job(id).await.unwrap();
            wait_for_job_state(&client, id, "Cancelled").await;
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}
