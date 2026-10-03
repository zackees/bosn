//! Setup ensure jobs: coalescing, ownership checks, persistence across restarts.

use super::*;

#[test]
fn setup_ensure_job_is_prompt_coalesced_bounded_and_cancellable_without_docker() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let fake = Arc::new(FakeSetupEnsureExecutor::new());
    RuntimeBuilder::multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_ensure_executor(fake.clone())
                    .serve(),
            );
            let client = wait_for_client(&state).await;
            let request = SetupEnsureJobRequest {
                workspace: workspace.clone(),
                config: "https://example.invalid/wait.toml".into(),
                policy: SetupPreparePolicy::Refresh,
                deadline: Duration::from_secs(2),
                output_limit: 4 * 1024,
            };
            let submitted = std::time::Instant::now();
            let first = client.submit_setup_ensure(request.clone()).await.unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(
                first,
                client.submit_setup_ensure(request.clone()).await.unwrap()
            );
            wait_for(|| fake.started.load(Ordering::SeqCst) == 1).await;
            client.ping().await.unwrap();
            let logs = wait_for_logs(&client, first).await;
            assert!(
                logs.records
                    .iter()
                    .all(|record| record.line.len() <= jobs::MAX_LOG_LINE_BYTES)
            );

            let changed = SetupEnsureJobRequest {
                config: "https://example.invalid/other.toml".into(),
                ..request
            };
            let second = client.submit_setup_ensure(changed).await.unwrap();
            assert_ne!(first, second);
            client.cancel_job(first).await.unwrap();
            wait_for_job_state(&client, first, "Cancelled").await;
            wait_for(|| fake.started.load(Ordering::SeqCst) == 2).await;
            wait_for_job_state(&client, second, "Succeeded").await;
            assert_eq!(fake.cancelled.load(Ordering::SeqCst), 1);
            assert_eq!(
                fake.stages(),
                vec![
                    "plan", "prepare", "ensure", "plan", "prepare", "ensure", "mutate"
                ]
            );
            client.shutdown().await.unwrap();
            stopped(server).await;
        });
    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    let events = registry.events(0, 10).unwrap().items;
    assert_eq!(
        events
            .iter()
            .map(|event| (event.kind.as_str(), event.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                "setup.ensure.submitted",
                "job_id=1 policy=refresh source=https",
            ),
            (
                "setup.ensure.submitted",
                "job_id=2 policy=refresh source=https",
            ),
            ("setup.ensure.cancelled", "job_id=1 outcome=cancelled"),
            ("setup.ensure.succeeded", "job_id=2 outcome=succeeded"),
        ]
    );
}

#[test]
fn setup_ensure_stops_after_prepare_failure_or_ownership_mismatch_without_mutation() {
    for config in [
        "https://user:secret@example.invalid/prepare-fail.toml?token=not-for-events",
        "https://user:secret@example.invalid/ensure-mismatch.toml?token=not-for-events",
    ] {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake = Arc::new(FakeSetupEnsureExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let job = client
                    .submit_setup_ensure(SetupEnsureJobRequest {
                        workspace,
                        config: config.into(),
                        policy: SetupPreparePolicy::Offline,
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
                        .any(|record| record.line.contains("setup ensure failed"))
                );
                assert!(!fake.stages().contains(&"mutate".into()));
                if config.contains("prepare-fail") {
                    assert_eq!(fake.stages(), vec!["plan", "prepare"]);
                } else {
                    assert_eq!(fake.stages(), vec!["plan", "prepare", "ensure"]);
                }
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
        let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
        assert!(registry.resources(0, 10).unwrap().items.is_empty());
        assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
        let events = registry.events(0, 10).unwrap().items;
        assert_eq!(
            events
                .iter()
                .map(|event| (event.kind.as_str(), event.detail.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "setup.ensure.submitted",
                    "job_id=1 policy=offline source=https",
                ),
                ("setup.ensure.failed", "job_id=1 outcome=failed"),
            ]
        );
        let rendered = format!("{events:?}");
        for sensitive in [
            "user:secret",
            "token=",
            "not-for-events",
            "prepare-fail",
            "ensure-mismatch",
        ] {
            assert!(!rendered.contains(sensitive), "event leaked {sensitive}");
        }
    }
}

#[test]
fn setup_ensure_persists_container_and_content_addressed_image_across_daemon_restart() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let request = SetupEnsureJobRequest {
        workspace: workspace.clone(),
        config: "https://example.invalid/setup.toml".into(),
        policy: SetupPreparePolicy::Refresh,
        deadline: Duration::from_secs(2),
        output_limit: 4 * 1024,
    };

    for _ in 0..2 {
        let fake = Arc::new(FakeSetupEnsureExecutor::new());
        RuntimeBuilder::multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .run(async {
                let server = async_engine::launch(
                    Service::new(state.clone())
                        .with_setup_ensure_executor(fake.clone())
                        .serve(),
                );
                let client = wait_for_client(&state).await;
                let job = client.submit_setup_ensure(request.clone()).await.unwrap();
                wait_for_job_state(&client, job, "Succeeded").await;
                client.shutdown().await.unwrap();
                stopped(server).await;
            });
    }

    let registry = Registry::open_read_only(state.join("registry.sqlite3")).unwrap();
    let resources = registry.resources(0, 10).unwrap().items;
    assert_eq!(resources.len(), 2);
    let container = resources
        .iter()
        .find(|resource| resource.kind == ResourceKind::Container)
        .unwrap();
    assert_eq!(container.id, "setup-container:fake");
    assert_eq!(container.name, "bosn-setup-fake");
    assert_eq!(container.stack, "setup");
    assert_eq!(container.generation, "sha256:fake");
    assert_eq!(container.scope, Scope::Machine);
    assert_eq!(container.workspace, workspace.to_string_lossy());
    assert_eq!(container.state, ResourceState::Active);
    assert_eq!(container.retention, Retention::Pinned);
    let image = resources
        .iter()
        .find(|resource| resource.kind == ResourceKind::Image)
        .unwrap();
    assert_eq!(image.id, "setup-image:sha256:fake");
    assert_eq!(image.name, "setup-image:sha256:fake");
    // Image generation preserves the verified inspected identity; it is
    // never a mutable tag or a caller-provided registry value.
    assert_eq!(image.generation, "sha256:fake");
    assert_eq!(image.scope, Scope::Machine);
    assert_eq!(image.workspace, workspace.to_string_lossy());
    assert_eq!(image.state, ResourceState::Active);
    assert_eq!(image.retention, Retention::Pinned);
    let uses = registry.resource_uses(0, 10).unwrap().items;
    assert_eq!(uses.len(), 2);
    for use_record in uses {
        assert_eq!(use_record.workspace, workspace.to_string_lossy());
        assert_eq!(use_record.stack, "setup");
        assert_eq!(use_record.state, ResourceState::Active);
    }
    let events = registry.events(0, 10).unwrap().items;
    assert_eq!(
        events
            .iter()
            .map(|event| (event.kind.as_str(), event.detail.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (
                "setup.ensure.submitted",
                "job_id=1 policy=refresh source=https",
            ),
            ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
            (
                "setup.ensure.submitted",
                "job_id=1 policy=refresh source=https",
            ),
            ("setup.ensure.succeeded", "job_id=1 outcome=succeeded"),
        ]
    );
}

#[test]
fn setup_image_registry_identity_is_content_addressed_for_pinned_and_inline_forms() {
    let workspace = "/verified/workspace";
    let identity = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let pinned = PreparedImage {
        setup_content_sha256: "pinned-document".into(),
        kind: PreparedImageKind::PinnedImage {
            image: "alpine@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .into(),
        },
        reference: "alpine@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        observed_identity: identity.into(),
    };
    let inline = PreparedImage {
        setup_content_sha256: "inline-document".into(),
        kind: PreparedImageKind::InlineDockerfile {
            tag: "bosn-setup:inline-document".into(),
        },
        reference: "bosn-setup:inline-document".into(),
        observed_identity: identity.into(),
    };
    let pinned_resource = setup_ensure_image_resource(&pinned, workspace);
    let inline_resource = setup_ensure_image_resource(&inline, workspace);
    // An immutable pulled image and an inline build which inspect to the
    // same local Docker image share exactly one machine resource. Mutable
    // references/tags never enter the durable identity.
    assert_eq!(pinned_resource, inline_resource);
    assert_eq!(pinned_resource.id, format!("setup-image:{identity}"));
    assert_eq!(pinned_resource.name, format!("setup-image:{identity}"));
    assert_eq!(pinned_resource.generation, identity);
    assert_eq!(pinned_resource.workspace, workspace);
}

#[test]
fn setup_ensure_registry_recording_is_atomic_when_image_identity_conflicts() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/verified/workspace";
    let image_name =
        "setup-image:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&Resource {
            id: "foreign-image-row".into(),
            kind: ResourceKind::Image,
            name: image_name.into(),
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

    let execution = SetupEnsureExecution {
        receipt: "ensured container".into(),
        resource: SetupEnsureResource {
            id: "setup-container:document".into(),
            name: "bosn-setup-document".into(),
            stack: "setup".into(),
            generation: "sha256:document".into(),
            workspace: workspace.into(),
        },
        image: SetupEnsureImageResource {
            id: "setup-image:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            name: image_name.into(),
            stack: "setup".into(),
            generation: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            workspace: workspace.into(),
        },
        volumes: Vec::new(),
        manifest_autostart: false,
    };
    assert!(matches!(
        record_setup_ensure(&mut registry, 7, &execution),
        Err(bosn_registry::Error::ResourceIdentityConflict)
    ));
    // The failed image upsert rolls back the preceding container and both
    // use rows; only the deliberate pre-existing conflicting row remains.
    let resources = registry.resources(0, 10).unwrap().items;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].id, "foreign-image-row");
    assert!(registry.resource_uses(0, 10).unwrap().items.is_empty());
    // The terminal success event is part of the same transaction as its
    // resources, so an identity conflict cannot leave a false success.
    assert!(registry.events(0, 10).unwrap().items.is_empty());
}

#[test]
fn setup_adoption_restores_absent_records_but_refuses_incompatible_existing_state() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let execution = setup_ensure_execution("/verified/workspace", "document", "sha256:image");
    record_setup_adoption(&mut registry, &execution).unwrap();
    assert_eq!(registry.resources(0, 10).unwrap().items.len(), 2);
    assert_eq!(registry.resource_uses(0, 10).unwrap().items.len(), 2);
    assert_eq!(
        registry.setup_ensure_events(0, 10).unwrap().items[0].kind,
        "setup.ensure.adopted"
    );
    // Same exact durable state is idempotent.
    record_setup_adoption(&mut registry, &execution).unwrap();
    let mut conflicting = execution.clone();
    conflicting.resource.workspace = "/other/workspace".into();
    assert!(matches!(
        record_setup_adoption(&mut registry, &conflicting),
        Err(bosn_registry::Error::ResourceIdentityConflict)
    ));
    let resources = registry.resources(0, 10).unwrap().items;
    assert_eq!(resources.len(), 2);
    assert!(
        resources
            .iter()
            .all(|resource| resource.workspace == "/verified/workspace")
    );
}

#[test]
#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn setup_ensure_generation_rollover_retires_only_prior_setup_containers() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let path = temporary.path().join("registry.sqlite3");
    let workspace_a = "/canonical/workspace-a";
    let workspace_b = "/canonical/workspace-b";
    let shared_image = "sha256:shared-image";
    let mut registry =
        Registry::create_writer(&path, "11111111-2222-4333-8444-555555555555").unwrap();

    record_setup_ensure(
        &mut registry,
        1,
        &setup_ensure_execution(workspace_a, "generation-a", shared_image),
    )
    .unwrap();

    // A container outside the `setup` stack is deliberately in the
    // product namespace too. Rollover must still leave it untouched.
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&Resource {
            id: "setup-container:other-stack".into(),
            kind: ResourceKind::Container,
            name: "other-stack-container".into(),
            stack: "other".into(),
            generation: "sha256:other".into(),
            scope: Scope::Machine,
            workspace: workspace_a.into(),
            created_at: 1.0,
            last_used: 1.0,
            state: ResourceState::Active,
            retention: Retention::Pinned,
        })
        .unwrap();
    transaction
        .put_resource_use(&ResourceUse {
            resource_id: "setup-container:other-stack".into(),
            workspace: workspace_a.into(),
            stack: "other".into(),
            generation: "sha256:other".into(),
            last_used: 1.0,
            state: ResourceState::Active,
        })
        .unwrap();
    transaction.commit().unwrap();
    // The registry primitive is deliberately hard-scoped to `setup`;
    // even an internal caller cannot reuse it to retire another stack.
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .retire_prior_setup_container_generations(workspace_a, "other", "sha256:new")
        .unwrap();
    transaction.commit().unwrap();

    // The exact same inspected image is shared across documents and
    // workspaces. It must never be retired during a container rollover.
    record_setup_ensure(
        &mut registry,
        2,
        &setup_ensure_execution(workspace_b, "generation-c", shared_image),
    )
    .unwrap();
    record_setup_ensure(
        &mut registry,
        3,
        &setup_ensure_execution(workspace_a, "generation-b", shared_image),
    )
    .unwrap();
    // Re-ensuring the current content is an active idempotent upsert, not
    // another retirement transition.
    record_setup_ensure(
        &mut registry,
        4,
        &setup_ensure_execution(workspace_a, "generation-b", shared_image),
    )
    .unwrap();
    drop(registry);

    // Reopen to prove terminal ownership accounting survives a daemon
    // restart rather than being an in-memory observation.
    let registry = Registry::open_read_only(&path).unwrap();
    let resources = registry.resources(0, 16).unwrap().items;
    let resource = |id: &str| resources.iter().find(|value| value.id == id).unwrap();
    assert_eq!(
        resource("setup-container:generation-a").state,
        ResourceState::Retired
    );
    assert_eq!(
        resource("setup-container:generation-b").state,
        ResourceState::Active
    );
    assert_eq!(
        resource("setup-container:generation-c").state,
        ResourceState::Active
    );
    assert_eq!(
        resource("setup-container:other-stack").state,
        ResourceState::Active
    );
    assert_eq!(
        resource(&format!("setup-image:{shared_image}")).state,
        ResourceState::Active
    );

    let uses = registry.resource_uses(0, 32).unwrap().items;
    let use_state = |id: &str, workspace: &str, stack: &str, generation: &str| {
        uses.iter()
            .find(|value| {
                value.resource_id == id
                    && value.workspace == workspace
                    && value.stack == stack
                    && value.generation == generation
            })
            .unwrap()
            .state
    };
    assert_eq!(
        use_state(
            "setup-container:generation-a",
            workspace_a,
            "setup",
            "sha256:generation-a"
        ),
        ResourceState::Retired
    );
    assert_eq!(
        use_state(
            "setup-container:generation-b",
            workspace_a,
            "setup",
            "sha256:generation-b"
        ),
        ResourceState::Active
    );
    assert_eq!(
        use_state(
            "setup-container:generation-c",
            workspace_b,
            "setup",
            "sha256:generation-c"
        ),
        ResourceState::Active
    );
    assert_eq!(
        use_state(
            "setup-container:other-stack",
            workspace_a,
            "other",
            "sha256:other"
        ),
        ResourceState::Active
    );
    for image_use in uses
        .iter()
        .filter(|value| value.resource_id == format!("setup-image:{shared_image}"))
    {
        assert_eq!(image_use.state, ResourceState::Active);
    }
}

#[test]
fn retired_stop_registry_confirmation_preserves_candidate_and_rejects_stale_state() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        temporary.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let workspace = "/canonical/retired-stop";
    record_setup_ensure(
        &mut registry,
        1,
        &setup_ensure_execution(workspace, "old", "sha256:image"),
    )
    .unwrap();
    record_setup_ensure(
        &mut registry,
        2,
        &setup_ensure_execution(workspace, "new", "sha256:image"),
    )
    .unwrap();
    let candidate = registry
        .setup_gc_candidate(
            workspace,
            "setup-container:old",
            "bosn-setup-old",
            "sha256:old",
        )
        .unwrap()
        .expect("retired candidate");
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        tx.confirm_setup_retired_container_stopped(
            workspace,
            &candidate.id,
            &candidate.name,
            &candidate.generation,
            3.0,
        )
        .unwrap()
    );
    tx.commit().unwrap();
    assert!(
        registry
            .setup_gc_candidate(
                workspace,
                &candidate.id,
                &candidate.name,
                &candidate.generation,
            )
            .unwrap()
            .is_some()
    );
    // A stale identity is a no-write failure, not permission to append an
    // event after a resource/use/lease/session protection race.
    let events_before = registry.events(0, 16).unwrap().items.len();
    let mut tx = registry.begin_immediate().unwrap();
    assert!(
        !tx.confirm_setup_retired_container_stopped(
            workspace,
            "setup-container:other",
            &candidate.name,
            &candidate.generation,
            4.0,
        )
        .unwrap()
    );
    drop(tx);
    assert_eq!(registry.events(0, 16).unwrap().items.len(), events_before);
}
