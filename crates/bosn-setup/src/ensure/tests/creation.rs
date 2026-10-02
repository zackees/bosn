//! Creation identity: workspace binding, sibling checkouts, actual configuration and volumes.

use super::*;

#[test]
fn creation_identity_binds_workspace_even_when_image_and_content_match() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::create_dir(first.path().join("src")).unwrap();
    std::fs::create_dir(second.path().join("src")).unwrap();
    let cancel = CancellationSource::new();
    let (events, _) = channel(8);
    let name = |workspace: &Path| {
        let plan = plan(workspace);
        let prepared = prepared(&plan);
        derive_command(&SetupEnsureRequest {
            plan: &plan,
            workspace_root: plan.workspace_root.clone(),
            prepared_image: &prepared,
            options: RunOptions::streaming(Duration::from_secs(2), 4096),
            cancellation: &cancel.token(),
            events: &events,
        })
        .unwrap()
        .container_name
    };
    assert_ne!(name(first.path()), name(second.path()));
    assert_eq!(name(first.path()), name(first.path()));
}

/// #314: two checkouts with an identical setup document must never share
/// a container, and a container carrying checkout A's name whose bind
/// mounts actually point at sibling checkout B (the pre-#349 shape, or a
/// stale/forged one) must not be adopted, so no task is ever exec'd into
/// another tree. The check covers both the actual `.Mounts[].Source` and
/// the declared `HostConfig.Mounts[].Source`, and neither adoption nor
/// ensure may mutate the foreign container.
#[test]
fn issue_314_sibling_checkout_never_shares_or_adopts_a_container() {
    let temporary = tempfile::tempdir().unwrap();
    let checkout_a = temporary.path().join("soldr-p2");
    let checkout_b = temporary.path().join("nixos-linker");
    for checkout in [&checkout_a, &checkout_b] {
        std::fs::create_dir_all(checkout.join("src")).unwrap();
    }
    let plan_a = plan(&checkout_a);
    let plan_b = plan(&checkout_b);
    assert_eq!(plan_a.content_sha256, plan_b.content_sha256);
    let image_a = prepared(&plan_a);
    let image_b = prepared(&plan_b);
    assert_eq!(image_a.observed_identity, image_b.observed_identity);
    let name_a = setup_container_name(&plan_a, &plan_a.workspace_root, &image_a).unwrap();
    let name_b = setup_container_name(&plan_b, &plan_b.workspace_root, &image_b).unwrap();
    assert_ne!(
        name_a, name_b,
        "identical manifests must not share a container"
    );

    // A container labelled and named for checkout A, but bound to B.
    let root_a = plan_a.workspace_root.to_str().unwrap().to_owned();
    let root_b = plan_b.workspace_root.to_str().unwrap().to_owned();
    let mut foreign = observed(&plan_a, true);
    assert_eq!(foreign.labels[LABEL_CONTAINER_NAME], name_a);
    let mut rebound = 0;
    for pointer in ["/Mounts", "/HostConfig/Mounts"] {
        for mount in foreign
            .configuration
            .pointer_mut(pointer)
            .and_then(serde_json::Value::as_array_mut)
            .unwrap()
        {
            if mount["Type"] == "bind" && mount["Source"] == root_a.as_str() {
                mount["Source"] = root_b.clone().into();
                rebound += 1;
            }
        }
    }
    assert_eq!(
        rebound, 2,
        "fixture must rebind actual and declared /workspace"
    );
    let derived = derive_creation(&plan_a, &plan_a.workspace_root, &image_a).unwrap();
    assert!(matches!(
        verify_actual_configuration(&foreign, &derived, &fixture_image()),
        Err(SetupEnsureError::OwnershipMismatch)
    ));

    let options = RunOptions::streaming(Duration::from_secs(2), 4096);
    let read_only = |engine: &FakeEngine| {
        engine.calls.lock().unwrap().iter().all(|call| {
            matches!(
                call,
                SetupEnsureCommand::Inspect { container_name } if container_name == &name_a
            ) || matches!(call, SetupEnsureCommand::ImageInspect { .. })
        })
    };
    // Adoption is the proof the daemon runs immediately before every
    // manifest task exec; it must refuse.
    let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
        Some(foreign.clone()),
        result(0, [], []),
    ))]);
    let cancellation = CancellationSource::new();
    assert!(matches!(
        run_adopt(
            &engine,
            &plan_a,
            &checkout_a,
            &image_a,
            &cancellation.token(),
            options
        ),
        Err(SetupEnsureError::OwnershipMismatch)
    ));
    assert!(read_only(&engine));
    // Ensure must refuse too, before any create/start/remove.
    let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
        Some(foreign),
        result(0, [], []),
    ))]);
    assert!(
        run(
            &engine,
            &plan_a,
            &checkout_a,
            &image_a,
            &cancellation.token(),
            options
        )
        .is_err()
    );
    assert!(read_only(&engine));
}

#[test]
fn correctly_labelled_wrong_actual_bind_or_execution_config_refuses() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    let plan = plan(workspace.path());
    let derived = derive_creation(&plan, workspace.path(), &prepared(&plan)).unwrap();
    let observed = observed(&plan, true);
    assert!(verify_actual_configuration(&observed, &derived, &fixture_image()).is_ok());
    for (pointer, replacement) in [
        ("/Mounts/0/Source", serde_json::json!("/foreign/workspace")),
        ("/Mounts/0/RW", serde_json::json!(false)),
        ("/Config/WorkingDir", serde_json::json!("/foreign")),
        ("/Config/Cmd", serde_json::json!(["wrong"])),
        ("/Config/Env", serde_json::json!(["PATH=/foreign"])),
        ("/HostConfig/Privileged", serde_json::json!(true)),
        ("/HostConfig/Tmpfs", serde_json::json!("malformed grant")),
        ("/HostConfig/AutoRemove", serde_json::json!(true)),
        ("/HostConfig/CgroupnsMode", serde_json::json!("host")),
    ] {
        let mut bad = observed.clone();
        *bad.configuration.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            matches!(
                verify_actual_configuration(&bad, &derived, &fixture_image()),
                Err(SetupEnsureError::OwnershipMismatch)
            ),
            "{pointer}"
        );
        let engine = FakeEngine::with_results([Ok(SetupEnsureResponse::Inspection(
            Some(bad),
            result(0, [], []),
        ))]);
        let cancellation = CancellationSource::new();
        assert!(
            run(
                &engine,
                &plan,
                workspace.path(),
                &prepared(&plan),
                &cancellation.token(),
                RunOptions::streaming(Duration::from_secs(2), 4096)
            )
            .is_err()
        );
        assert!(engine.calls.lock().unwrap().iter().all(|c| matches!(
            c,
            SetupEnsureCommand::Inspect { .. } | SetupEnsureCommand::ImageInspect { .. }
        )));
    }
    let mut legacy = observed.clone();
    legacy.labels.remove(LABEL_CREATION_PROFILE);
    assert!(matches!(
        validate_observed(&legacy, &derived),
        Err(SetupEnsureError::OwnershipMismatch)
    ));
}

#[test]
fn creation_identity_and_actual_verification_bind_named_volume_set() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join("src")).unwrap();
    let mut plan = plan(workspace.path());
    plan.named_volumes.push(crate::SetupNamedVolume {
        name: "bosn-v-stack-a".into(),
        target: "/target".into(),
        labels: BTreeMap::from([
            (LABEL_MANAGED.into(), MANAGED_VALUE.into()),
            (LABEL_CONTENT_SHA256.into(), HASH.into()),
            (LABEL_CONTAINER_NAME.into(), "bosn-v-stack-a".into()),
        ]),
    });
    let derived = derive_creation(&plan, workspace.path(), &prepared(&plan)).unwrap();
    let observed = observed(&plan, true);
    assert!(verify_actual_configuration(&observed, &derived, &fixture_image()).is_ok());
    let mut bad = observed.clone();
    bad.configuration["Mounts"][1]["Name"] = "bosn-v-stack-foreign".into();
    assert!(verify_actual_configuration(&bad, &derived, &fixture_image()).is_err());
    plan.named_volumes[0].name = "bosn-v-stack-b".into();
    plan.named_volumes[0]
        .labels
        .insert(LABEL_CONTAINER_NAME.into(), "bosn-v-stack-b".into());
    assert_ne!(
        derived.container_name,
        setup_container_name(&plan, workspace.path(), &prepared(&plan)).unwrap()
    );
}

#[test]
fn guest_image_storage_volume_requires_exact_explicit_attachment() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = macos_guest_plan(workspace.path());
    let image = prepared(&plan);
    let expected = derive_creation(&plan, workspace.path(), &image).unwrap();
    let mut base = fixture_image();
    base["Config"]["Volumes"] = serde_json::json!({"/storage":{}});
    let mut observation = observed(&plan, false);
    observation.configuration["Config"]["Volumes"] = base["Config"]["Volumes"].clone();
    assert!(verify_actual_configuration(&observation, &expected, &base).is_ok());
    base["Config"]["Volumes"]["/anonymous"] = serde_json::json!({});
    observation.configuration["Config"]["Volumes"] = base["Config"]["Volumes"].clone();
    assert!(verify_actual_configuration(&observation, &expected, &base).is_err());
}

#[test]
fn reuse_and_adoption_refuse_local_volume_bind_options_or_source_mismatch() {
    let workspace = tempfile::tempdir().unwrap();
    let plan = macos_guest_plan(workspace.path());
    let image = prepared(&plan);
    let volume = &plan.named_volumes[0];
    for bind_backed in [true, false] {
        let mut observation = observed(&plan, true);
        observation.configuration["Mounts"][0]["Source"] =
            serde_json::json!("/var/lib/docker/volumes/owned/_data");
        let receipt = serde_json::json!({"Name":volume.name,"Driver":"local","Labels":volume.labels,
            "Scope":"local","Mountpoint":if bind_backed { "/var/lib/docker/volumes/owned/_data" } else { "/foreign/path" },
            "Options":if bind_backed { serde_json::json!({"type":"none","o":"bind","device":"/foreign/path"}) } else { serde_json::Value::Null }});
        for adopt in [false, true] {
            let engine = FakeEngine::with_results([
                Ok(SetupEnsureResponse::Inspection(
                    Some(observation.clone()),
                    result(0, [], []),
                )),
                command(serde_json::to_vec(&receipt).unwrap()),
            ]);
            let cancellation = CancellationSource::new();
            let options = RunOptions::streaming(Duration::from_secs(2), 8192);
            let result = if adopt {
                run_adopt(
                    &engine,
                    &plan,
                    workspace.path(),
                    &image,
                    &cancellation.token(),
                    options,
                )
            } else {
                run(
                    &engine,
                    &plan,
                    workspace.path(),
                    &image,
                    &cancellation.token(),
                    options,
                )
            };
            assert!(
                matches!(result, Err(SetupEnsureError::OwnershipMismatch)),
                "bind_backed={bind_backed} adopt={adopt}"
            );
            assert!(engine.calls.lock().unwrap().iter().all(|c| matches!(
                c,
                SetupEnsureCommand::Inspect { .. }
                    | SetupEnsureCommand::ImageInspect { .. }
                    | SetupEnsureCommand::VolumeInspect { .. }
            )));
        }
    }
}
