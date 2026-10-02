//! Manifest ensure, converge, rollover and daemon-start recovery.

use super::*;

#[test]
fn manifest_ensure_is_daemon_owned_and_records_atomic_facts_with_fake_executor() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(FakeManifestEnsureExecutor::default());
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(2),
                output_limit: 4096,
            };
            let first = client.submit_manifest_ensure(request).await.unwrap();
            wait_for_job_state(&client, first, "Succeeded").await;
            assert_eq!(fake.calls.lock().unwrap().len(), 1);
            let resources = client.registry_resources(0, 16).await.unwrap();
            assert!(resources.records.iter().any(|record| record.stack == "app"));
            let events = Registry::open_read_only(state.join("registry.sqlite3"))
                .unwrap()
                .events(0, 16)
                .unwrap();
            assert!(
                events
                    .items
                    .iter()
                    .any(|record| record.kind == "manifest.ensure.succeeded")
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn daemon_start_reproves_then_starts_only_an_exact_stopped_manifest_container() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\ndefault = true\n"),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let request = ManifestEnsureJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: Duration::from_secs(2),
            output_limit: 4096,
        };
        let plan = manifest_stack_setup_plan(&request).await.unwrap();
        let content = plan.generation.strip_prefix("sha256:").unwrap();
        let prepared = recovery_fixture_image(&plan.plan, TEST_IDENTITY);
        let proof = recovery_fixture_proof(&plan.plan, &prepared);
        let name = bosn_setup::setup_container_name(&plan.plan, &workspace, &prepared).unwrap();
        let execution = SetupEnsureExecution {
            receipt: "seed".into(),
            resource: SetupEnsureResource {
                id: format!("manifest-container:app:{}", plan.generation),
                name: name.clone(),
                stack: "app".into(),
                generation: plan.generation.clone(),
                workspace: plan.plan.workspace_root.to_string_lossy().into_owned(),
            },
            image: SetupEnsureImageResource {
                id: "manifest-image:verified-fixture".into(),
                name: "manifest-image:verified-fixture".into(),
                stack: "app".into(),
                generation: TEST_IDENTITY.into(),
                workspace: plan.plan.workspace_root.to_string_lossy().into_owned(),
            },
            volumes: Vec::new(),
            manifest_autostart: true,
        };
        let contract = manifest_recovery_contract(&request, &execution, 1).unwrap();
        let mut registry = Registry::create_writer(
            state.join("registry.sqlite3"),
            "11111111-2222-4333-8444-555555555555",
        )
        .unwrap();
        record_manifest_ensure(&mut registry, 1, &execution, &contract).unwrap();
        drop(registry);
        let fake = Arc::new(FakeManifestRecoveryExecutor {
            observed: Mutex::new(Some(SetupReconcileObserved {
                name: format!("/{name}"),
                running: false,
                image_identity: contract.image_identity.clone(),
                managed: "v1".into(),
                content: content.into(),
                container: name,
            })),
            starts: AtomicUsize::new(0),
            proof: Some(proof),
        });
        let server = async_engine::launch(
            Service::new(state.clone())
                .with_manifest_recovery_executor(fake.clone())
                .serve(),
        );
        let client = wait_for_client(&state).await;
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        assert!(fake.observed.lock().unwrap().as_ref().unwrap().running);
        client.shutdown().await.unwrap();
        stopped(server).await;
        let events = Registry::open_read_only(state.join("registry.sqlite3"))
            .unwrap()
            .events(0, 32)
            .unwrap();
        assert!(
            events
                .items
                .iter()
                .any(|event| event.kind == "manifest.recovery.started")
        );
    });
}

#[test]
fn daemon_start_refuses_manifest_source_drift_without_inspecting_or_starting() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\ndefault = true\n"),
    )
    .unwrap();
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let request = ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(2),
                output_limit: 4096,
            };
            let plan = manifest_stack_setup_plan(&request).await.unwrap();
            let content = plan.generation.strip_prefix("sha256:").unwrap();
            let name = format!("bosn-setup-recovery-{content}");
            let execution = SetupEnsureExecution {
                receipt: "seed".into(),
                resource: SetupEnsureResource {
                    id: format!("manifest-container:app:{}", plan.generation),
                    name: name.clone(),
                    stack: "app".into(),
                    generation: plan.generation.clone(),
                    workspace: plan.plan.workspace_root.to_string_lossy().into_owned(),
                },
                image: SetupEnsureImageResource {
                    id: "manifest-image:sha256:recovery-test".into(),
                    name: "manifest-image:sha256:recovery-test".into(),
                    stack: "app".into(),
                    generation: "sha256:recovery-test".into(),
                    workspace: plan.plan.workspace_root.to_string_lossy().into_owned(),
                },
                volumes: Vec::new(),
                manifest_autostart: true,
            };
            let contract = manifest_recovery_contract(&request, &execution, 1).unwrap();
            let mut registry = Registry::create_writer(
                state.join("registry.sqlite3"),
                "11111111-2222-4333-8444-555555555555",
            )
            .unwrap();
            record_manifest_ensure(&mut registry, 1, &execution, &contract).unwrap();
            drop(registry);
            // Changing a declared environment member changes the native
            // runtime generation. Recovery must refuse before inspecting
            // the old container, let alone starting it.
            std::fs::write(
                workspace.join("bosn.toml"),
                format!("[stack.app]\nimage = '{image}'\ndefault = true\n[stack.app.env]\nCHANGED = 'yes'\n"),
            )
            .unwrap();
            let fake = Arc::new(FakeManifestRecoveryExecutor {
                observed: Mutex::new(Some(SetupReconcileObserved {
                    name: format!("/{name}"),
                    running: false,
                    image_identity: contract.image_identity.clone(),
                    managed: "v1".into(),
                    content: content.into(),
                    container: name.clone(),
                })),
                starts: AtomicUsize::new(0),
                proof: None,
            });
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_recovery_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
            assert!(!fake.observed.lock().unwrap().as_ref().unwrap().running);
            client.shutdown().await.unwrap();
            stopped(server).await;
            let events = Registry::open_read_only(state.join("registry.sqlite3"))
                .unwrap()
                .events(0, 32)
                .unwrap();
            assert!(
                events
                    .items
                    .iter()
                    .any(|event| event.kind == "manifest.recovery.refused_source")
            );
            assert!(
                events
                    .items
                    .iter()
                    .any(|event| event.kind == "manifest.autostart.disabled")
            );
            // The durable veto is consulted before source proof on the
            // next daemon start. A changed/missing worktree therefore
            // cannot keep generating engine inspection attempts.
            let vetoed = Arc::new(FakeManifestRecoveryExecutor {
                observed: Mutex::new(Some(SetupReconcileObserved {
                    name: format!("/{name}"),
                    running: false,
                    image_identity: contract.image_identity.clone(),
                    managed: "v1".into(),
                    content: contract
                        .generation
                        .strip_prefix("sha256:")
                        .unwrap()
                        .into(),
                    container: name.clone(),
                })),
                starts: AtomicUsize::new(0),
                proof: None,
            });
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_recovery_executor(vetoed.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            assert_eq!(vetoed.starts.load(Ordering::SeqCst), 0);
            client.shutdown().await.unwrap();
            stopped(server).await;
            let events = Registry::open_read_only(state.join("registry.sqlite3"))
                .unwrap()
                .setup_ensure_events(0, 32)
                .unwrap();
            assert!(
                events
                    .items
                    .iter()
                    .any(|event| event.kind == "manifest.autostart.already_disabled")
            );
        });
}

#[test]
fn manifest_converge_orders_all_stacks_and_records_each_before_completion() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.zebra]\nimage = '{image}'\n[stack.alpha]\nimage = '{image}'\n"),
    )
    .unwrap();
    let fake = Arc::new(FakeManifestEnsureExecutor::default());
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let job = client
                .submit_manifest_converge(ManifestConvergeJobRequest {
                    workspace: workspace.clone(),
                    manifest: "bosn.toml".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                })
                .await
                .unwrap();
            wait_for_job_state(&client, job, "Succeeded").await;
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.stack.as_str())
                    .collect::<Vec<_>>(),
                ["alpha", "zebra"]
            );
            let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
            let resources = registry.resources(0, 16).unwrap().items;
            assert!(resources.iter().any(|resource| resource.stack == "alpha"));
            assert!(resources.iter().any(|resource| resource.stack == "zebra"));
            let logs = client.job_logs(job, 0, 64).await.unwrap();
            assert!(
                logs.records
                    .iter()
                    .any(|record| record.line.contains("stack alpha (1/2)"))
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn manifest_converge_order_is_schema_determined_and_dependency_spellings_fail_closed() {
    let roots = ManifestRoots::new("test", "/material", "/workspace");
    let manifest = parse_manifest_toml(
        "[stack.z]\nimage='example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.a]\nimage='example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n",
        roots.clone(),
    )
    .unwrap();
    assert_eq!(manifest_converge_stack_order(&manifest), ["a", "z"]);
    assert!(parse_manifest_toml(
        "[stack.app]\nimage='example@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\ndepends_on=['db']\n",
        roots,
    )
    .is_err());
}

#[test]
fn manifest_converge_stops_at_failure_and_keeps_prior_stack_registry_facts() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.alpha]\nimage = '{image}'\n[stack.broken]\nimage = '{image}'\n[stack.later]\nimage = '{image}'\n"),
    )
    .unwrap();
    let fake = Arc::new(FakeManifestEnsureExecutor {
        calls: Mutex::new(Vec::new()),
        fail_stack: Mutex::new(Some("broken".into())),
    });
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let job = client
                .submit_manifest_converge(ManifestConvergeJobRequest {
                    workspace: workspace.clone(),
                    manifest: "bosn.toml".into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                })
                .await
                .unwrap();
            wait_for_job_state(&client, job, "Failed").await;
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request.stack.as_str())
                    .collect::<Vec<_>>(),
                ["alpha", "broken"]
            );
            let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
            let resources = registry.resources(0, 16).unwrap().items;
            assert!(resources.iter().any(|resource| resource.stack == "alpha"));
            assert!(!resources.iter().any(|resource| resource.stack == "broken"));
            assert!(!resources.iter().any(|resource| resource.stack == "later"));
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn manifest_ensure_rollover_is_same_stack_only_and_same_generation_reuses_identity() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace_a = temporary.path().join("workspace-a");
    let workspace_b = temporary.path().join("workspace-b");
    std::fs::create_dir(&workspace_a).unwrap();
    std::fs::create_dir(&workspace_b).unwrap();
    let fake = Arc::new(FakeManifestEnsureExecutor::default());
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_manifest_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request =
                |workspace: PathBuf, manifest: &str, stack: &str| ManifestEnsureJobRequest {
                    workspace,
                    manifest: manifest.into(),
                    stack: stack.into(),
                    deadline: Duration::from_secs(2),
                    output_limit: 4096,
                };
            let old = client
                .submit_manifest_ensure(request(workspace_a.clone(), "old.toml", "app"))
                .await
                .unwrap();
            wait_for_job_state(&client, old, "Succeeded").await;
            // A completed same-generation request is a new durable job,
            // but reuses exactly its existing managed identity.
            let old_again = client
                .submit_manifest_ensure(request(workspace_a.clone(), "old.toml", "app"))
                .await
                .unwrap();
            wait_for_job_state(&client, old_again, "Succeeded").await;
            let other_workspace = client
                .submit_manifest_ensure(request(workspace_b.clone(), "workspace-b.toml", "app"))
                .await
                .unwrap();
            wait_for_job_state(&client, other_workspace, "Succeeded").await;
            let other_stack = client
                .submit_manifest_ensure(request(workspace_a.clone(), "other-stack.toml", "other"))
                .await
                .unwrap();
            wait_for_job_state(&client, other_stack, "Succeeded").await;
            let new = client
                .submit_manifest_ensure(request(workspace_a.clone(), "new.toml", "app"))
                .await
                .unwrap();
            wait_for_job_state(&client, new, "Succeeded").await;

            let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
            let resources = registry.resources(0, 32).unwrap().items;
            let find = |id: &str| resources.iter().find(|resource| resource.id == id).unwrap();
            assert_eq!(
                find("manifest-container:app:old").state,
                ResourceState::Retired
            );
            assert_eq!(
                find("manifest-container:app:new").state,
                ResourceState::Active
            );
            assert_eq!(
                find("manifest-container:app:workspace-b").state,
                ResourceState::Active,
                "other workspace must not be retired"
            );
            assert_eq!(
                find("manifest-container:other:other-stack").state,
                ResourceState::Active,
                "other stack must not be retired"
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 5);
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
}

#[test]
fn manifest_rollover_is_atomic_when_current_image_identity_conflicts() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/canonical/manifest";
    let old = manifest_ensure_execution(workspace, "app", "old", "sha256:old-image");
    record_manifest_ensure(
        &mut registry,
        1,
        &old,
        &test_manifest_recovery_contract(&old),
    )
    .unwrap();
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&Resource {
            id: "foreign-image".into(),
            kind: ResourceKind::Image,
            name: "manifest-image:sha256:new-image".into(),
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
    let new = manifest_ensure_execution(workspace, "app", "new", "sha256:new-image");
    assert!(matches!(
        record_manifest_ensure(
            &mut registry,
            2,
            &new,
            &test_manifest_recovery_contract(&new),
        ),
        Err(bosn_registry::Error::ResourceIdentityConflict)
    ));
    let resources = registry.resources(0, 16).unwrap().items;
    assert_eq!(
        resources
            .iter()
            .find(|resource| resource.id == "manifest-container:app:old")
            .unwrap()
            .state,
        ResourceState::Active,
        "a failed new record cannot retire the previous generation"
    );
    assert!(
        resources
            .iter()
            .all(|resource| resource.id != "manifest-container:app:new")
    );
    assert_eq!(
        registry
            .events(0, 16)
            .unwrap()
            .items
            .iter()
            .filter(|event| event.kind == "manifest.ensure.succeeded")
            .count(),
        1,
        "the failed operation cannot leave a terminal success event"
    );
}

#[test]
fn manifest_volume_intent_precedes_engine_work_and_is_consumed_with_success() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/canonical/manifest";
    let volume = manifest_volume_resource(workspace, "app");

    put_manifest_volume_intents(&mut registry, std::slice::from_ref(&volume)).unwrap();
    let intents = registry.volume_creation_intents(0, 8).unwrap().items;
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].name, volume.name);
    assert_eq!(intents[0].labels, volume.labels);

    let mut execution = manifest_ensure_execution(workspace, "app", "generation", "sha256:image");
    execution.volumes.push(volume.clone());
    record_manifest_ensure(
        &mut registry,
        1,
        &execution,
        &test_manifest_recovery_contract(&execution),
    )
    .unwrap();

    assert!(
        registry
            .volume_creation_intents(0, 8)
            .unwrap()
            .items
            .is_empty()
    );
    let resources = registry.resources(0, 8).unwrap().items;
    let recorded = resources
        .iter()
        .find(|resource| resource.id == volume.id)
        .unwrap();
    assert_eq!(recorded.kind, ResourceKind::Volume);
    assert_eq!(recorded.name, volume.name);
    assert_eq!(recorded.state, ResourceState::Active);
    assert!(
        registry
            .resource_uses(0, 8)
            .unwrap()
            .items
            .iter()
            .any(|use_record| use_record.resource_id == volume.id)
    );
}

#[test]
fn manifest_restart_authorization_refuses_pending_volume_intents_and_uncertain_sessions() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let execution =
        manifest_ensure_execution("/canonical/manifest", "app", "generation", "sha256:image");
    let contract = test_manifest_recovery_contract(&execution);
    record_manifest_ensure(&mut registry, 1, &execution, &contract).unwrap();
    let authorized = |registry: &Registry| {
        registry
            .manifest_recovery_container_active(
                &contract.resource_id,
                &contract.name,
                &contract.stack,
                &contract.generation,
                &contract.workspace,
                &manifest_autostart_intent_detail(&contract),
            )
            .unwrap()
    };
    assert!(authorized(&registry));
    let volume = manifest_volume_resource(&contract.workspace, &contract.stack);
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_volume_creation_intent(&VolumeCreationIntent {
            name: volume.name.clone(),
            labels: volume.labels.clone(),
            stack: contract.stack.clone(),
            generation: volume.generation,
            scope: volume.scope,
            workspace: contract.workspace.clone(),
        })
        .unwrap();
    transaction.commit().unwrap();
    assert!(
        !authorized(&registry),
        "a crashed volume create must block restart"
    );
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .delete_volume_creation_intent(&volume.name)
        .unwrap();
    transaction
        .put_execution_session(&ExecutionSession {
            id: "uncertain-manifest-task".into(),
            container_id: contract.name.clone(),
            engine_binary: "docker".into(),
            client_pid: 1,
            client_start: None,
            lease_ids: Vec::new(),
        })
        .unwrap();
    transaction.commit().unwrap();
    assert!(
        !authorized(&registry),
        "an uncertain task session must block restart"
    );
}
