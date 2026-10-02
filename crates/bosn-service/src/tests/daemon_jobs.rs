//! Typed jobs through a live daemon: prepare, task and app-task coalescing and sessions.

use super::*;

#[test]
fn fresh_daemon_serves_typed_client_and_releases_writer() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
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
            assert_eq!(logs.records[0].line, "[fake] preparation started");

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
    let fake = Arc::new(FakeSetupAppTaskExecutor::new());
    RuntimeBuilder::multi_thread()
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
            assert_eq!(
                first,
                client.submit_setup_app_task(request).await.unwrap(),
                "identical semantic app-task requests coalesce"
            );
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
    let fake = Arc::new(FakeManifestAppTaskExecutor::new());
    RuntimeBuilder::multi_thread()
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
            assert_eq!(
                first,
                client.submit_manifest_app_task(request).await.unwrap()
            );
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
    assert_eq!(registry.status().unwrap().sessions, 1);
    assert_eq!(
        registry.execution_sessions(0, 1).unwrap().items[0].container_id,
        container_name
    );
    let protected = registry.setup_gc_preview("/workspace", 0, 16).unwrap();
    assert!(protected.candidates.items.is_empty());
    assert_eq!(protected.counts.protected_session, 1);

    finish_setup_app_task_session(&mut registry, 7, "failed").unwrap();
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
