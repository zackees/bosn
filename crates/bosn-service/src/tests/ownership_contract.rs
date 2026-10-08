//! Production creation metadata must satisfy managed retention.

use super::*;

#[test]
fn recording_prepared_image_preserves_an_existing_explicit_pin() {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let mut registry = Registry::create_writer(
        root.path().join("registry.sqlite3"),
        "11111111-2222-4333-8444-555555555555",
    )
    .unwrap();
    let identity = format!("sha256:{}", "a".repeat(64));
    let image = SetupEnsureImageResource {
        id: format!("setup-image:{identity}"),
        name: format!("setup-image:{identity}"),
        stack: "setup".into(),
        generation: identity,
        workspace: "/fixture".into(),
    };
    record_prepared_image(&mut registry, &image).unwrap();
    let mut resource = registry.resources(0, 1).unwrap().items.remove(0);
    assert_eq!(resource.retention, Retention::Warm);
    resource.retention = Retention::Pinned;
    let mut transaction = registry.begin_immediate().unwrap();
    transaction.put_resource(&resource).unwrap();
    transaction.commit().unwrap();
    record_prepared_image(&mut registry, &image).unwrap();
    let refreshed = registry.resources(0, 1).unwrap().items.remove(0);
    assert_eq!(refreshed.retention, Retention::Pinned);
    assert_eq!(refreshed.generation, image.generation);
    assert!(refreshed.last_used >= resource.last_used);
}

/// #545: exercise the production planner, rather than inventing GC-compatible labels.
#[test]
fn manifest_volume_creation_contract_is_discoverable_by_managed_gc() {
    let root = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = root.path().join("workspace");
    let state = root.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let owner = "11111111-2222-4333-8444-555555555555";
    let mut registry =
        bosn_registry::Registry::create_writer(state.join("registry.sqlite3"), owner).unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = 'example.invalid/app@sha256:{}'\n             [stack.app.volumes]\ncache = {{ scope = 'stack', destination = '/cache' }}\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let planned = runtime
        .run(manifest_stack_setup_plan_at(
            &ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(5),
                output_limit: 1024,
            },
            Some(&state),
        ))
        .unwrap();
    let volume = &planned.volumes[0];
    let mut transaction = registry.begin_immediate().unwrap();
    transaction
        .put_resource(&bosn_registry::Resource {
            id: volume.id.clone(),
            kind: bosn_core::ResourceKind::Volume,
            name: volume.name.clone(),
            stack: volume.stack.clone(),
            generation: volume.generation.clone(),
            scope: volume.scope,
            workspace: volume.workspace.clone(),
            created_at: 1.0,
            last_used: 1.0,
            state: bosn_core::ResourceState::Active,
            retention: volume.retention,
        })
        .unwrap();
    transaction.commit().unwrap();
    let ownership =
        crate::managed_retention::registered::RegisteredOwnership::load(&state).unwrap();
    let (labels, _) = ownership
        .normalize(
            bosn_core::ResourceKind::Volume,
            &volume.name,
            &volume.labels,
        )
        .expect("production labels plus their durable registration must enter managed GC");
    let artifact = bosn_core::ObservedArtifact {
        id: volume.name.clone(),
        kind: bosn_core::ResourceKind::Volume,
        labels,
        signals: bosn_core::Signals::default(),
        bytes: Some(1024),
        age_seconds: Some(31.0 * 86400.0),
    };
    assert_eq!(
        bosn_core::retention::classify_managed(
            &artifact,
            Some(owner),
            bosn_core::retention::RetentionPolicy::default(),
        )
        .hold,
        None,
        "GC must accept the actual production ownership contract"
    );
    drop(registry);
}
