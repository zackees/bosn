//! Setup ensure generation rollover, cancellation and wire bounds.

use super::*;

#[derive(Default)]
struct RejectPreparedOwnership {
    images: std::sync::Mutex<Vec<SetupEnsureImageResource>>,
    containers: std::sync::Mutex<Vec<SetupEnsureResource>>,
    reject_container: bool,
    reject_preparation: bool,
    delay_preparation: bool,
}
impl SetupImageRecorder for RejectPreparedOwnership {
    fn record_preparation<'a>(
        &'a self,
        _intent: bosn_registry::ImageCreationIntent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if self.delay_preparation {
                async_engine::sleep(Duration::from_millis(20)).await;
            }
            if self.reject_preparation {
                Err("preparation checkpoint rejected".into())
            } else {
                Ok(())
            }
        })
    }
    fn record<'a>(
        &'a self,
        image: SetupEnsureImageResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.images.lock().unwrap().push(image);
            if self.reject_container {
                Ok(())
            } else {
                Err("ownership checkpoint rejected".into())
            }
        })
    }
    fn record_container_intent<'a>(
        &'a self,
        container: SetupEnsureResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.containers.lock().unwrap().push(container);
            Err("ownership checkpoint rejected".into())
        })
    }
}

#[test]
fn ensure_requires_ownership_checkpoints_before_any_container_operation() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let plan = pipeline_plan(&workspace);
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    for reject_container in [false, true] {
        for owner in [
            PreparedImageOwner::Setup,
            PreparedImageOwner::Manifest("app"),
        ] {
            let (namespace, stack) = match &owner {
                PreparedImageOwner::Setup => ("setup-image", "setup"),
                PreparedImageOwner::Manifest(stack) => ("manifest-image", *stack),
            };
            let engine = PipelineFakeEngine::new(
                [
                    Ok(command_result(0, Vec::new())),
                    Ok(command_result(0, format!("{TEST_IDENTITY}\n"))),
                ],
                [],
            );
            let recorder = RejectPreparedOwnership {
                reject_container,
                ..Default::default()
            };
            runtime.run(async {
                let deadline = async_engine::Deadline::after(Duration::from_secs(1));
                let cancellation = CancellationSource::new();
                let (events, _event_receiver) = async_engine::channel(8);
                let (text_logs, _log_receiver) = async_engine::channel(8);
                let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
                let pipeline = SetupEnsurePipeline {
                    plan: &plan,
                    workspace: workspace.clone(),
                    deadline: &deadline,
                    prepare_output: 512,
                    ensure_output: 512,
                    images: Some((&recorder, owner)),
                };
                let error = execute_setup_ensure_pipeline(
                    &engine,
                    &pipeline,
                    &cancellation.token(),
                    &events,
                    &logs,
                )
                .await
                .err()
                .unwrap();
                assert_eq!(error, "ownership checkpoint rejected");
            });
            assert!(engine.ensure_calls.lock().unwrap().is_empty());
            let records = recorder.images.lock().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].generation, TEST_IDENTITY);
            assert_eq!(records[0].name, records[0].id);
            assert_eq!(records[0].stack, stack);
            assert_eq!(records[0].id, format!("{namespace}:{TEST_IDENTITY}"));
            let containers = recorder.containers.lock().unwrap();
            assert_eq!(containers.len(), usize::from(reject_container));
            if reject_container {
                let container = &containers[0];
                let namespace = if stack == "setup" {
                    "setup-container"
                } else {
                    "manifest-container"
                };
                assert_eq!(container.stack, stack);
                assert_eq!(
                    container.generation,
                    format!("sha256:{}", plan.content_sha256)
                );
                assert_eq!(container.workspace, plan.workspace_root.to_string_lossy());
                assert_eq!(
                    container.id,
                    setup_container_resource_id(namespace, stack, &container.name)
                );
                assert!(!container.name.is_empty());
            }
        }
    }
}

#[test]
fn setup_ensure_rollover_conflict_rolls_back_without_retiring_current_generation() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/canonical/workspace";
    record_setup_ensure(
        &mut registry,
        1,
        &setup_ensure_execution(workspace, "generation-a", "sha256:image-a"),
    )
    .unwrap();

    // Make the later image upsert fail after the next generation's
    // container would otherwise have been accepted. The immediate
    // transaction must preserve the active old generation and its use.
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&Resource {
            id: "foreign-image".into(),
            kind: ResourceKind::Image,
            name: "setup-image:sha256:image-b".into(),
            stack: "foreign".into(),
            generation: "sha256:foreign".into(),
            scope: Scope::Machine,
            workspace: workspace.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
    transaction.commit().unwrap();
    assert!(matches!(
        record_setup_ensure(
            &mut registry,
            2,
            &setup_ensure_execution(workspace, "generation-b", "sha256:image-b"),
        ),
        Err(bosn_registry::Error::ResourceIdentityConflict)
    ));
    let old = registry
        .resources(0, 16)
        .unwrap()
        .items
        .into_iter()
        .find(|value| value.id == "setup-container:generation-a")
        .unwrap();
    assert_eq!(old.state, ResourceState::Active);
    let old_use = registry
        .resource_uses(0, 16)
        .unwrap()
        .items
        .into_iter()
        .find(|value| value.resource_id == "setup-container:generation-a")
        .unwrap();
    assert_eq!(old_use.state, ResourceState::Active);
    assert!(
        registry
            .resources(0, 16)
            .unwrap()
            .items
            .iter()
            .all(|value| value.id != "setup-container:generation-b")
    );
}

#[test]
fn setup_ensure_rollover_never_retires_a_container_shared_by_another_workspace() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace_a = "/canonical/workspace-a";
    let workspace_b = "/canonical/workspace-b";
    record_setup_ensure(
        &mut registry,
        1,
        &setup_ensure_execution(workspace_a, "generation-a", "sha256:image-a"),
    )
    .unwrap();
    // This is not normal setup-app ownership (the content-addressed
    // container should not be shared across workspaces), but it proves
    // that accounting fails closed rather than retiring a global resource
    // observed by another workspace.
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource_use(&ResourceUse {
            resource_id: "setup-container:generation-a".into(),
            workspace: workspace_b.into(),
            stack: "setup".into(),
            generation: "sha256:generation-a".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    transaction.commit().unwrap();
    record_setup_ensure(
        &mut registry,
        2,
        &setup_ensure_execution(workspace_a, "generation-b", "sha256:image-b"),
    )
    .unwrap();

    let resources = registry.resources(0, 16).unwrap().items;
    assert_eq!(
        resources
            .iter()
            .find(|value| value.id == "setup-container:generation-a")
            .unwrap()
            .state,
        ResourceState::Active
    );
    let uses = registry.resource_uses(0, 16).unwrap().items;
    assert!(
        uses.iter()
            .filter(|value| value.resource_id == "setup-container:generation-a")
            .all(|value| value.state == ResourceState::Active)
    );
    assert_eq!(
        resources
            .iter()
            .find(|value| value.id == "setup-container:generation-b")
            .unwrap()
            .state,
        ResourceState::Active
    );
}

#[test]
fn setup_ensure_rollover_is_visible_through_daemon_registry_diagnostics() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let workspace = "/canonical/workspace";
    let mut registry = Registry::create_writer(
        state.join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    record_setup_ensure(
        &mut registry,
        1,
        &setup_ensure_execution(workspace, "generation-a", "sha256:image"),
    )
    .unwrap();
    record_setup_ensure(
        &mut registry,
        2,
        &setup_ensure_execution(workspace, "generation-b", "sha256:image"),
    )
    .unwrap();
    drop(registry);

    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_act_backend(Arc::new(crate::ci::lifecycle::tests::FakeBackend::with(
                        Default::default(),
                    )))
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let page = client.registry_resources(0, 16).await.unwrap();
            assert_eq!(
                page.records
                    .iter()
                    .find(|value| value.id == "setup-container:generation-a")
                    .unwrap()
                    .state,
                "retired"
            );
            assert_eq!(
                page.records
                    .iter()
                    .find(|value| value.id == "setup-container:generation-b")
                    .unwrap()
                    .state,
                "active"
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn cancelled_setup_ensure_does_not_persist_or_retire_existing_resources() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let canonical_workspace = workspace.to_string_lossy().into_owned();
    let mut initial_registry = Registry::create_writer(
        state.join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    record_setup_ensure(
        &mut initial_registry,
        1,
        &setup_ensure_execution(
            &canonical_workspace,
            "existing-generation",
            "sha256:existing-image",
        ),
    )
    .unwrap();
    drop(initial_registry);
    let fake = Arc::new(FakeSetupEnsureExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_act_backend(Arc::new(crate::ci::lifecycle::tests::FakeBackend::with(
                        Default::default(),
                    )))
                    .with_setup_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let job = client
                .submit_setup_ensure(SetupEnsureJobRequest {
                    workspace,
                    config: "https://example.invalid/wait.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                })
                .await
                .unwrap();
            wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
            client.cancel_job(job).await.unwrap();
            wait_for_job_state(&client, job, "Cancelled").await;
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    let resources = registry.resources(0, 10).unwrap().items;
    assert_eq!(resources.len(), 2);
    assert!(
        resources
            .iter()
            .all(|resource| resource.state == ResourceState::Active)
    );
    let uses = registry.resource_uses(0, 10).unwrap().items;
    assert_eq!(uses.len(), 2);
    assert!(
        uses.iter()
            .all(|resource_use| resource_use.state == ResourceState::Active)
    );
    assert_eq!(
        registry
            .events(0, 10)
            .unwrap()
            .items
            .iter()
            .map(|event| (event.kind.as_str(), event.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
            (
                "setup.ensure.submitted",
                "job_id=1 policy=refresh source=https",
            ),
            ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
        ]
    );
}

#[test]
fn shutdown_cancelled_setup_ensure_keeps_a_durable_terminal_event() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(FakeSetupEnsureExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_act_backend(Arc::new(crate::ci::lifecycle::tests::FakeBackend::with(
                        Default::default(),
                    )))
                    .with_setup_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            client
                .submit_setup_ensure(SetupEnsureJobRequest {
                    workspace,
                    config: "https://example.invalid/wait.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                })
                .await
                .unwrap();
            wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert!(registry.resources(0, 10).unwrap().items.is_empty());
    assert_eq!(
        registry
            .events(0, 10)
            .unwrap()
            .items
            .iter()
            .map(|event| (event.kind.as_str(), event.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                "setup.ensure.submitted",
                "job_id=1 policy=refresh source=https",
            ),
            ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
        ]
    );
}

#[test]
fn cancellation_queued_at_registry_handoff_is_rejected_after_persisted_success() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let registry = Registry::create_writer(
                state.join("registry.sqlite3"),
                "11111111-2222-4333-8444-555555555555",
            )
            .unwrap();
            let (entered, mut entered_wait) = async_engine::channel(1);
            let (release, release_wait) = async_engine::channel(1);
            let (registry_sender, registry_receiver) = async_engine::channel(4);
            let registry_handle = RegistryActor {
                sender: registry_sender,
            };
            let registry_task = async_engine::launch(registry_actor(
                registry,
                registry_receiver,
                Some(SetupEnsureRecordGate {
                    entered,
                    release: release_wait,
                }),
            ));
            let (job_sender, job_receiver) = async_engine::channel(4);
            let jobs = JobActor {
                sender: job_sender.clone(),
            };
            let fake = Arc::new(FakeSetupEnsureExecutor::new());
            let job_task = async_engine::launch(job_actor(
                Jobs::new(1),
                job_receiver,
                SetupExecutors {
                    state_dir: None,
                    prepare: Arc::new(SlowFakeSetupExecutor::new()),
                    task: Arc::new(FakeSetupTaskExecutor::new()),
                    app_task: Arc::new(FakeSetupAppTaskExecutor::new(None)),
                    ensure: fake,
                    manifest_ensure: Arc::new(DockerManifestEnsureExecutor::new(state.clone())),
                    manifest_app_task: Arc::new(DockerManifestAppTaskExecutor::new(state.clone())),
                    runners: None,
                },
                job_sender.clone(),
                registry_handle.clone(),
            ));
            let id = jobs
                .submit_setup_ensure(SetupEnsureJobRequest {
                    workspace: workspace.clone(),
                    config: "https://example.invalid/setup.toml".into(),
                    policy: SetupPreparePolicy::Refresh,
                    deadline: Duration::from_secs(2),
                    output_limit: 4 * 1024,
                })
                .await
                .unwrap();
            assert!(entered_wait.recv().await.is_some());

            // This command is now definitely queued behind an in-flight
            // actor-owned registry transaction, not merely racing a token
            // check in a worker task.
            let (reply, wait) = async_engine::oneshot_channel();
            assert!(
                job_sender
                    .send(JobCommand::Cancel { id, reply })
                    .await
                    .is_ok()
            );
            release.send(()).await.unwrap();
            assert!(wait.await.unwrap().is_err());
            assert_eq!(
                jobs.status(id).await.unwrap().state,
                jobs::JobState::Succeeded
            );

            jobs.stop().await;
            registry_handle.stop().await;
            job_task.await.unwrap();
            registry_task.await.unwrap();
        });
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    assert_eq!(registry.resources(0, 10).unwrap().items.len(), 2);
    assert_eq!(registry.resource_uses(0, 10).unwrap().items.len(), 2);
}

#[test]
fn setup_ensure_coalescing_digest_covers_every_immutable_input() {
    let base = SetupEnsureJobRequest {
        workspace: PathBuf::from("/workspace"),
        config: "https://example.invalid/setup.toml".into(),
        policy: SetupPreparePolicy::Refresh,
        deadline: Duration::from_secs(2),
        output_limit: 4 * 1024,
    };
    let variants = [
        SetupEnsureJobRequest {
            workspace: PathBuf::from("/other"),
            ..base.clone()
        },
        SetupEnsureJobRequest {
            config: "https://example.invalid/other.toml".into(),
            ..base.clone()
        },
        SetupEnsureJobRequest {
            policy: SetupPreparePolicy::Offline,
            ..base.clone()
        },
        SetupEnsureJobRequest {
            deadline: Duration::from_secs(3),
            ..base.clone()
        },
        SetupEnsureJobRequest {
            output_limit: 8 * 1024,
            ..base.clone()
        },
    ];
    for variant in variants {
        assert_ne!(setup_ensure_digest(&base), setup_ensure_digest(&variant));
    }
}

#[test]
fn setup_ensure_wire_validation_preserves_prepare_bounds() {
    assert!(
        validate_setup_ensure_wire(
            "/workspace",
            "https://example.invalid/setup.toml",
            SetupPreparePolicy::Refresh,
            1,
            1,
        )
        .is_ok()
    );
    for (deadline, output) in [(0, 1), (300_001, 1), (1, 0), (1, 8_388_609)] {
        assert!(
            validate_setup_ensure_wire(
                "/workspace",
                "https://example.invalid/setup.toml",
                SetupPreparePolicy::Offline,
                deadline,
                output,
            )
            .is_err()
        );
    }
}

#[test]
fn setup_ensure_rejects_all_legacy_job_and_task_wire_fields() {
    let request = || Request {
        workspace: "/workspace".into(),
        setup_config: "https://example.invalid/setup.toml".into(),
        setup_policy: SetupPreparePolicy::Refresh.wire(),
        setup_deadline_ms: 1,
        setup_output_limit: 1,
        ..Request::operation(10)
    };
    assert!(validate_setup_ensure_request_wire(&request(), SetupPreparePolicy::Refresh).is_ok());
    for invalid in [
        Request {
            stack: "stack".into(),
            ..request()
        },
        Request {
            digest: "digest".into(),
            ..request()
        },
        Request {
            job_id: 1,
            ..request()
        },
        Request {
            log_after: 1,
            ..request()
        },
        Request {
            log_limit: 1,
            ..request()
        },
        Request {
            setup_task_name: "task".into(),
            ..request()
        },
    ] {
        assert!(validate_setup_ensure_request_wire(&invalid, SetupPreparePolicy::Refresh).is_err());
    }
}

#[test]
fn registry_diagnostics_wire_rejects_nonsemantic_or_unbounded_fields() {
    let request = || Request {
        diagnostic_after: 0,
        diagnostic_limit: 1,
        ..Request::operation(11)
    };
    assert!(validate_registry_diagnostics_request_wire(&request()).is_ok());
    for invalid in [
        Request {
            workspace: "/attacker".into(),
            ..request()
        },
        Request {
            setup_config: "https://user:secret@example.invalid/setup.toml".into(),
            ..request()
        },
        Request {
            job_id: 1,
            ..request()
        },
        Request {
            diagnostic_limit: MAX_REGISTRY_DIAGNOSTIC_PAGE + 1,
            ..request()
        },
    ] {
        assert!(validate_registry_diagnostics_request_wire(&invalid).is_err());
    }
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn ensure_pipeline_does_not_reset_budget_and_never_mutates_after_prepare_or_ownership_failure() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let plan = pipeline_plan(&workspace);
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();

    // A failed image action prevents all ensure inspection/create/start
    // operations, so no container mutation can follow preparation failure.
    let prepare_failure =
        PipelineFakeEngine::new([Ok(command_result(1, b"pull failed".to_vec()))], []);
    runtime.run(async {
        let deadline = async_engine::Deadline::after(Duration::from_secs(1));
        let cancellation = CancellationSource::new();
        let (events, _event_receiver) = async_engine::channel(8);
        let (text_logs, _log_receiver) = async_engine::channel(8);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let pipeline = SetupEnsurePipeline {
            plan: &plan,
            workspace: workspace.clone(),
            deadline: &deadline,
            prepare_output: 512,
            ensure_output: 512,
            images: None,
        };
        assert!(
            execute_setup_ensure_pipeline(
                &prepare_failure,
                &pipeline,
                &cancellation.token(),
                &events,
                &logs,
            )
            .await
            .is_err()
        );
    });
    assert!(prepare_failure.ensure_calls.lock().unwrap().is_empty());

    // A mismatching observed candidate fails in ensure_setup_app before
    // any create/start command. The fake has no later mutation response
    // configured, making an accidental mutation an immediate test failure.
    let mismatch = PipelineFakeEngine::new(
        [
            Ok(command_result(0, Vec::new())),
            Ok(command_result(0, format!("{TEST_IDENTITY}\n"))),
        ],
        [Ok(bosn_setup::SetupEnsureResponse::Inspection(
            Some(bosn_setup::SetupEnsureObservedContainer {
                container_id: TEST_CONTAINER_ID.into(),
                running: false,
                image_identity:
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into(),
                labels: BTreeMap::new(),
                configuration: serde_json::Value::Null,
            }),
            command_result(0, Vec::new()),
        ))],
    );
    runtime.run(async {
        let deadline = async_engine::Deadline::after(Duration::from_secs(1));
        let cancellation = CancellationSource::new();
        let (events, _event_receiver) = async_engine::channel(8);
        let (text_logs, _log_receiver) = async_engine::channel(8);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let pipeline = SetupEnsurePipeline {
            plan: &plan,
            workspace: workspace.clone(),
            deadline: &deadline,
            prepare_output: 512,
            ensure_output: 512,
            images: None,
        };
        assert!(
            execute_setup_ensure_pipeline(
                &mismatch,
                &pipeline,
                &cancellation.token(),
                &events,
                &logs,
            )
            .await
            .is_err()
        );
    });
    assert_eq!(mismatch.ensure_calls.lock().unwrap().len(), 1);

    let image = recovery_fixture_image(&plan, TEST_IDENTITY);
    let (observed, image_config, _) = recovery_fixture_proof(&plan, &image);
    let success = PipelineFakeEngine::new(
        [
            Ok(command_result(0, Vec::new())),
            Ok(command_result(0, format!("{TEST_IDENTITY}\n"))),
        ],
        [
            Ok(bosn_setup::SetupEnsureResponse::Inspection(
                None,
                command_result(1, Vec::new()),
            )),
            Ok(bosn_setup::SetupEnsureResponse::Command(command_result(
                0,
                format!("{TEST_CONTAINER_ID}\n"),
            ))),
            Ok(bosn_setup::SetupEnsureResponse::Inspection(
                Some(observed),
                command_result(0, Vec::new()),
            )),
            Ok(bosn_setup::SetupEnsureResponse::Command(command_result(
                0,
                serde_json::to_vec(&image_config).unwrap(),
            ))),
            Ok(bosn_setup::SetupEnsureResponse::Command(command_result(
                0,
                Vec::new(),
            ))),
        ],
    );
    runtime.run(async {
        let deadline = async_engine::Deadline::after(Duration::from_secs(1));
        let cancellation = CancellationSource::new();
        let (events, _event_receiver) = async_engine::channel(8);
        let (text_logs, _log_receiver) = async_engine::channel(8);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let pipeline = SetupEnsurePipeline {
            plan: &plan,
            workspace,
            deadline: &deadline,
            prepare_output: 1024,
            ensure_output: 1025,
            images: None,
        };
        execute_setup_ensure_pipeline(&success, &pipeline, &cancellation.token(), &events, &logs)
            .await
            .unwrap();
    });
    // The prepare helper uses its one 1024-byte allocation for pull and
    // inspect; ensure receives the disjoint 1025-byte remainder, never
    // the caller's full 2049-byte cap again.
    assert_eq!(success.image_calls.lock().unwrap()[0].1.output_limit, 1024);
    assert_eq!(success.ensure_calls.lock().unwrap()[0].1.output_limit, 1025);
}

#[test]
fn preparation_checkpoint_failure_prevents_all_engine_operations() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let plan = pipeline_plan(temporary.path());
    let engine = PipelineFakeEngine::new([], []);
    let recorder = RejectPreparedOwnership {
        reject_preparation: true,
        ..Default::default()
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let deadline = async_engine::Deadline::after(Duration::from_secs(1));
        let cancellation = CancellationSource::new();
        let (events, _receiver) = async_engine::channel(8);
        let (text_logs, _logs) = async_engine::channel(8);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let pipeline = SetupEnsurePipeline {
            plan: &plan,
            workspace: temporary.path().into(),
            deadline: &deadline,
            prepare_output: 512,
            ensure_output: 512,
            images: Some((&recorder, PreparedImageOwner::Setup)),
        };
        let error = execute_setup_ensure_pipeline(
            &engine,
            &pipeline,
            &cancellation.token(),
            &events,
            &logs,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error, "preparation checkpoint rejected");
    });
    assert!(engine.image_calls.lock().unwrap().is_empty());
    assert!(engine.ensure_calls.lock().unwrap().is_empty());
}

#[test]
fn preparation_checkpoint_latency_cannot_extend_engine_deadline() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let plan = pipeline_plan(temporary.path());
    let engine = PipelineFakeEngine::new([], []);
    let recorder = RejectPreparedOwnership {
        delay_preparation: true,
        ..Default::default()
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let deadline = async_engine::Deadline::after(Duration::from_millis(10));
        let cancellation = CancellationSource::new();
        let (events, _receiver) = async_engine::channel(8);
        let (text_logs, _logs) = async_engine::channel(8);
        let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
        let pipeline = SetupEnsurePipeline {
            plan: &plan,
            workspace: temporary.path().into(),
            deadline: &deadline,
            prepare_output: 512,
            ensure_output: 512,
            images: Some((&recorder, PreparedImageOwner::Setup)),
        };
        let error = execute_setup_ensure_pipeline(
            &engine,
            &pipeline,
            &cancellation.token(),
            &events,
            &logs,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error, "setup image preparation exceeded its deadline");
    });
    assert!(engine.image_calls.lock().unwrap().is_empty());
    assert!(engine.ensure_calls.lock().unwrap().is_empty());
}
