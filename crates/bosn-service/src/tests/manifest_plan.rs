//! Manifest wires and stack planning (volumes, tmpfs, binds, Dockerfiles, guests).

use super::*;

#[test]
fn manifest_ensure_wire_accepts_only_its_typed_selectors() {
    let valid = || Request {
        workspace: "/workspace".into(),
        setup_config: "bosn.toml".into(),
        stack: "app_one".into(),
        setup_deadline_ms: 1,
        setup_output_limit: 1,
        ..Request::operation(22)
    };
    assert!(validate_manifest_ensure_request_wire(&valid()).is_ok());
    for invalid in [
        Request {
            setup_config: "../bosn.toml".into(),
            ..valid()
        },
        Request {
            setup_policy: 1,
            ..valid()
        },
        Request {
            setup_task_name: "shell".into(),
            ..valid()
        },
    ] {
        assert!(validate_manifest_ensure_request_wire(&invalid).is_err());
    }
}

#[test]
fn manifest_app_task_wire_accepts_only_declared_task_selectors() {
    let valid = || Request {
        workspace: "/workspace".into(),
        setup_config: "bosn.toml".into(),
        stack: "app_one".into(),
        setup_task_name: "check-1".into(),
        setup_deadline_ms: 1,
        setup_output_limit: 1,
        ..Request::operation(23)
    };
    assert!(validate_manifest_app_task_request_wire(&valid()).is_ok());
    for invalid in [
        Request {
            setup_config: "../bosn.toml".into(),
            ..valid()
        },
        Request {
            setup_policy: 1,
            ..valid()
        },
        Request {
            setup_task_name: "shell; id".into(),
            ..valid()
        },
    ] {
        assert!(validate_manifest_app_task_request_wire(&invalid).is_err());
    }
}

#[test]
fn manifest_stack_plan_derives_generation_and_refuses_unsupported_fields() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\n[stack.app.env]\nMODE = 'test'\n"),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ManifestRuntimePlan {
        plan, generation, ..
    } = runtime
        .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        }))
        .unwrap();
    assert!(generation.starts_with("sha256:"));
    assert_eq!(plan.app.environment["MODE"], "test");
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\nworkdir = '/'\n"),
    )
    .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .is_err()
    );
}

#[test]
fn manifest_startup_selection_uses_only_existing_default_stack_semantics() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.web]\nimage = '{image}'\ndefault = true\n[stack.worker]\nimage = '{image}'\n"
        ),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (stack, expected) in [("web", true), ("worker", false)] {
        let plan = runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: "bosn.toml".into(),
                stack: stack.into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap();
        assert_eq!(plan.autostart, expected, "{stack}");
    }
}

#[test]
fn manifest_macos_guest_preflight_uses_kernel_os_facades_and_conservative_default() {
    let manifest = parse_manifest_toml(
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.volumes.storage]\nscope = 'machine'\ndestination = '/storage'\nretention = 'pinned'\n",
        ManifestRoots::new("test", "/workspace", "/workspace"),
    )
    .unwrap();
    let stack = manifest.stack("mac").unwrap();
    let unsupported = ManifestGuestHostCapability {
        os: "macos",
        kvm_available: false,
        tun_available: false,
    };
    assert!(derive_manifest_macos_guest(stack, &unsupported).is_err());
    let capable = ManifestGuestHostCapability {
        os: "linux",
        kvm_available: true,
        tun_available: true,
    };
    assert_eq!(
        derive_manifest_macos_guest(stack, &capable)
            .unwrap()
            .unwrap()
            .cpu_cores,
        1
    );
    assert!(derive_manifest_macos_guest(stack, &capable).is_ok());
    let non_loopback = parse_manifest_toml(
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.guest]\nssh_host = 'guest.example'\n[stack.mac.volumes.storage]\nscope = 'machine'\ndestination = '/storage'\nretention = 'pinned'\n",
        ManifestRoots::new("test", "/workspace", "/workspace"),
    )
    .unwrap();
    assert!(
        derive_manifest_macos_guest(non_loopback.stack("mac").unwrap(), &capable)
            .unwrap_err()
            .contains("127.0.0.1")
    );
}

#[test]
fn manifest_macos_guest_accepts_only_dockurr_digest_and_exact_storage_contract() {
    let capable = ManifestGuestHostCapability {
        os: "linux",
        kvm_available: true,
        tun_available: true,
    };
    let manifest = |body: &str| {
        parse_manifest_toml(body, ManifestRoots::new("test", "/workspace", "/workspace")).unwrap()
    };
    for image in [
        "dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "docker.io/dockurr/macos:latest@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "index.docker.io/dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "registry-1.docker.io/dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(valid_manifest_macos_guest_image(image), "{image}");
    }
    for image in [
        "registry.example/dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "docker.io/evil/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "dockurr/macos@sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        assert!(!valid_manifest_macos_guest_image(image), "{image}");
    }

    let valid = manifest(
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.volumes.storage]\nscope = 'machine'\ndestination = '/storage'\nretention = 'pinned'\n",
    );
    assert!(derive_manifest_macos_guest(valid.stack("mac").unwrap(), &capable).is_ok());

    for body in [
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'example.invalid/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.volumes.storage]\nscope = 'machine'\ndestination = '/storage'\nretention = 'pinned'\n",
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n",
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.volumes.disk]\nscope = 'machine'\ndestination = '/storage'\nretention = 'pinned'\n",
        "[stack.mac]\nkind = 'macos-x64-guest'\nacknowledge_macos_license = true\nimage = 'dockurr/macos@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n[stack.mac.volumes.storage]\nscope = 'stack'\ndestination = '/storage'\nretention = 'pinned'\n",
    ] {
        let invalid = manifest(body);
        assert!(derive_manifest_macos_guest(invalid.stack("mac").unwrap(), &capable).is_err());
    }
}

#[test]
fn manifest_dockerfile_plan_materializes_one_selected_context_and_rolls_generation() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'Dockerfile'\n[task.check]\nstack = 'app'\ncmd = 'test -f /payload'\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM scratch\nCOPY payload /payload\n",
    )
    .unwrap();
    std::fs::write(workspace.join("payload"), "one\n").unwrap();
    let request = ManifestEnsureJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let first = runtime
        .run(manifest_stack_setup_plan_at(&request, Some(&state)))
        .unwrap();
    assert!(matches!(
        first.plan.app.source,
        SetupSource::InlineDockerfile(_)
    ));
    let asset_root = first.plan.asset_root.as_ref().unwrap();
    assert_eq!(
        asset_root.file_name().and_then(|name| name.to_str()),
        Some(first.plan.content_sha256.as_str())
    );
    assert_eq!(
        std::fs::read_to_string(asset_root.join("payload")).unwrap(),
        "one\n"
    );
    assert_eq!(
        first.plan.app_source,
        SetupPlanAppSource::InlineDockerfile {
            dockerfile_path: asset_root.join("Dockerfile"),
        }
    );
    std::fs::write(workspace.join("payload"), "two\n").unwrap();
    let second = runtime
        .run(manifest_stack_setup_plan_at(&request, Some(&state)))
        .unwrap();
    assert_ne!(first.generation, second.generation);
    assert_ne!(first.plan.content_sha256, second.plan.content_sha256);
    assert_eq!(
        std::fs::read_to_string(second.plan.asset_root.unwrap().join("payload")).unwrap(),
        "two\n"
    );

    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM alpine:3.21\nCOPY payload /payload\n",
    )
    .unwrap();
    let unpinned = match runtime.run(manifest_stack_setup_plan_at(&request, Some(&state))) {
        Err(error) => error,
        Ok(_) => panic!("a tag-only FROM is refused"),
    };
    // The refusal names the reference and the exact remedy.
    assert!(
        unpinned.contains("Dockerfile uses alpine:3.21"),
        "{unpinned}"
    );
    assert!(
        unpinned.contains("FROM alpine:3.21@sha256:<digest>"),
        "{unpinned}"
    );
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\ndockerfile = 'Dockerfile'\nimage = 'example.invalid/app@sha256:{}'\n",
            "a".repeat(64)
        ),
    )
    .unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM scratch\nCOPY payload /payload\n",
    )
    .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan_at(&request, Some(&state)))
            .is_err()
    );
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'Dockerfile'\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM scratch\nCOPY ../outside /outside\n",
    )
    .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan_at(&request, Some(&state)))
            .is_err()
    );
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'docker/Dockerfile'\n",
    )
    .unwrap();
    std::fs::create_dir(workspace.join("docker")).unwrap();
    std::fs::write(
        workspace.join("docker/Dockerfile"),
        "FROM scratch\nCOPY payload /payload\n",
    )
    .unwrap();
    let alternate = runtime
        .run(manifest_stack_setup_plan_at(&request, Some(&state)))
        .unwrap();
    let alternate_root = alternate.plan.asset_root.unwrap();
    assert_eq!(
        alternate.plan.app_source,
        SetupPlanAppSource::InlineDockerfile {
            dockerfile_path: alternate_root.join("docker/Dockerfile"),
        }
    );
}

#[cfg(unix)]
#[test]
fn manifest_dockerfile_plan_refuses_links_until_kernel_can_create_them_and_preserves_empty_directories()
 {
    use std::os::unix::fs::symlink;

    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        "[stack.app]\ndockerfile = 'Dockerfile'\n",
    )
    .unwrap();
    std::fs::write(workspace.join("payload"), "ok\n").unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM scratch\nCOPY link /payload\n",
    )
    .unwrap();
    symlink("payload", workspace.join("link")).unwrap();
    let request = ManifestEnsureJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan_at(&request, Some(&state)))
            .is_err()
    );
    std::fs::remove_file(workspace.join("link")).unwrap();
    std::fs::create_dir(workspace.join("empty")).unwrap();
    std::fs::write(
        workspace.join("Dockerfile"),
        "FROM scratch\nCOPY empty /empty\n",
    )
    .unwrap();
    let plan = runtime
        .run(manifest_stack_setup_plan_at(&request, Some(&state)))
        .unwrap();
    let asset_root = plan.plan.asset_root.unwrap();
    assert_eq!(
        kernal_api::platform::fs::context_path_metadata_no_follow(&asset_root.join("empty"))
            .unwrap()
            .kind,
        kernal_api::platform::fs::ContextPathKind::Directory
    );
}

#[test]
fn manifest_stack_plan_derives_typed_scoped_named_volumes() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\nfamily = 'shared-cache'\n[stack.app.volumes.cache]\nscope = 'stack'\ndestination = '/var/cache/app'\nretention = 'pinned'\n[stack.app.volumes.scratch]\nscope = 'spec'\n"
        ),
    ).unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
            workspace: workspace.clone(),
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        }))
        .unwrap();
    assert_eq!(result.plan.named_volumes.len(), 2);
    assert_eq!(result.volumes.len(), 2);
    assert!(result.plan.named_volumes.iter().all(|volume| {
        volume.name.starts_with("bosn-v-")
            && volume
                .labels
                .get("com.zackees.bosn.setup-content-sha256")
                .is_some_and(|identity| identity.len() == 64)
    }));
    let cache = result
        .volumes
        .iter()
        .find(|volume| volume.retention == Retention::Pinned)
        .unwrap();
    assert_eq!(cache.scope, Scope::Stack);
    assert_eq!(cache.workspace, workspace.to_string_lossy());

    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\n[stack.app.env]\nMODE = 'changed'\n[stack.app.volumes.cache]\nscope = 'stack'\ndestination = '/var/cache/app'\nretention = 'pinned'\n[stack.app.volumes.scratch]\nscope = 'spec'\n"
        ),
    )
    .unwrap();
    let changed = runtime
        .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
            workspace,
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        }))
        .unwrap();
    assert_ne!(changed.generation, result.generation);
    let original_stack = result
        .volumes
        .iter()
        .find(|volume| volume.scope == Scope::Stack)
        .unwrap();
    let changed_stack = changed
        .volumes
        .iter()
        .find(|volume| volume.scope == Scope::Stack)
        .unwrap();
    assert_eq!(changed_stack.name, original_stack.name);
    assert_eq!(changed_stack.generation, original_stack.generation);
}

#[test]
fn manifest_stack_plan_derives_only_typed_tmpfs_and_rolls_generation() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    let request = || ManifestEnsureJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\ntmpfs = ['/run/cache:ro,size=64m']\n"),
    )
    .unwrap();
    let first = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(first.plan.tmpfs.len(), 1);
    assert_eq!(first.plan.tmpfs[0].target, "/run/cache");
    assert!(first.plan.tmpfs[0].readonly);
    assert_eq!(
        first.plan.tmpfs[0].size,
        Some(SetupTmpfsSize {
            value: 64,
            unit: SetupTmpfsSizeUnit::Mebibytes,
        })
    );
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\ntmpfs = ['/run/cache:rw,size=64m']\n"),
    )
    .unwrap();
    let changed = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_ne!(first.generation, changed.generation);

    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\ntmpfs = ['/run/cache:rw,size=64m,exec,mode=1777']\n"
        ),
    )
    .unwrap();
    let executable = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(executable.plan.tmpfs[0].exec, Some(true));
    assert_eq!(executable.plan.tmpfs[0].mode, Some(0o1777));
    assert_ne!(executable.generation, changed.generation);
    std::fs::write(
        workspace.join("bosn.toml"),
        format!("[stack.app]\nimage = '{image}'\ntmpfs = ['/run/cache:noexec']\n"),
    )
    .unwrap();
    let noexec = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(noexec.plan.tmpfs[0].exec, Some(false));
    assert_eq!(noexec.plan.tmpfs[0].mode, None);

    for declaration in [
        "['/run/cache:ro,rw']",
        "['/run/cache:size=64m,size=32m']",
        "['/run/cache:exec,noexec']",
        "['/run/cache:mode=8']",
        "['/run/cache:mode=17777']",
        "['/run/cache:mode=1777,mode=1777']",
        "['/run/cache:uid=0']",
        "['/run/cache:size=0m']",
        "['/run/cache:size=1t']",
        "['/one', '/one/']",
    ] {
        std::fs::write(
            workspace.join("bosn.toml"),
            format!("[stack.app]\nimage = '{image}'\ntmpfs = {declaration}\n"),
        )
        .unwrap();
        assert!(runtime.run(manifest_stack_setup_plan(&request())).is_err());
    }
}

#[test]
fn manifest_host_docker_socket_is_typed_and_other_host_paths_explain_the_remedy() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let outside = temporary.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    let request = || ManifestEnsureJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let write = |source: &str| {
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\n[stack.app.mounts.source]\nsource = '.'\ndestination = '/workspace'\nreadonly = true\n[stack.app.mounts.docker]\nsource = '{source}'\ndestination = '/var/run/docker.sock'\n"
            ),
        )
        .unwrap();
    };

    write(&outside.to_string_lossy());
    let refused = match runtime.run(manifest_stack_setup_plan(&request())) {
        Err(error) => error,
        Ok(_) => panic!("a host path outside the workspace is refused"),
    };
    assert!(refused.contains("escapes workspace"), "{refused}");
    assert!(refused.contains("/var/run/docker.sock"), "{refused}");
    assert!(refused.contains("[stack.NAME.volumes]"), "{refused}");

    write("/var/run/docker.sock");
    let host_has_socket = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata("/var/run/docker.sock")
                .is_ok_and(|metadata| metadata.file_type().is_socket())
        }
        #[cfg(not(unix))]
        {
            false
        }
    };
    match runtime.run(manifest_stack_setup_plan(&request())) {
        Ok(plan) => {
            assert!(host_has_socket);
            // Typed, never a workspace bind.
            assert_eq!(
                plan.plan.host_docker_socket,
                Some(SetupHostDockerSocket {
                    source: SetupHostDockerSocketSource::VarRun,
                    target: "/var/run/docker.sock".into(),
                    readonly: false,
                })
            );
            assert_eq!(plan.plan.app.mounts.len(), 1);
            assert_eq!(plan.plan.app.mounts[0].target, "/workspace");
            assert_eq!(
                plan.plan.app.command.as_deref(),
                Some(MANIFEST_LINUX_IDLE_COMMAND)
            );
        }
        Err(error) => {
            assert!(!host_has_socket, "{error}");
            assert!(error.contains("host Docker socket"), "{error}");
        }
    }
}

#[test]
fn manifest_stack_plan_translates_workspace_binds_and_workdir_into_setup_shape() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("project")).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    let write = |workdir: &str, readonly: bool| {
        std::fs::write(
            workspace.join("bosn.toml"),
            format!(
                "[stack.app]\nimage = '{image}'\nworkdir = '{workdir}'\n[stack.app.mounts.repo]\nsource = '.'\ndestination = '/repo'\nreadonly = {readonly}\n[stack.app.mounts.project]\nsource = 'project'\ndestination = '/repo/project'\n"
            ),
        )
        .unwrap();
    };
    let request = || ManifestEnsureJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    write("/repo/project", true);
    let ManifestRuntimePlan {
        plan: first,
        generation: first_generation,
        ..
    } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(first.app.workdir.as_deref(), Some("project"));
    assert_eq!(
        first.app.mounts,
        vec![
            bosn_core::WorkspaceMount {
                source: "project".into(),
                target: "/repo/project".into(),
                readonly: false,
            },
            bosn_core::WorkspaceMount {
                source: ".".into(),
                target: "/repo".into(),
                readonly: true,
            },
        ]
    );
    write("/repo/project", false);
    let ManifestRuntimePlan {
        plan: second,
        generation: second_generation,
        ..
    } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(second.app.workdir.as_deref(), Some("project"));
    assert_ne!(first_generation, second_generation);
    assert_ne!(first.content_sha256, second.content_sha256);
    write("/repo", false);
    let ManifestRuntimePlan {
        plan: third,
        generation: third_generation,
        ..
    } = runtime.run(manifest_stack_setup_plan(&request())).unwrap();
    assert_eq!(third.app.workdir.as_deref(), Some("."));
    assert_ne!(second_generation, third_generation);
    assert_ne!(second.content_sha256, third.content_sha256);
}

#[test]
fn manifest_stack_plan_refuses_mount_sources_outside_the_selected_workspace() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    let outside = temporary.path().join("outside");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\n[stack.app.mounts.bad]\nsource = '{}'\ndestination = '/repo'\n",
            outside.display()
        ),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn manifest_stack_plan_refuses_a_symlink_mount_source_even_when_it_names_workspace() {
    use std::os::unix::fs::symlink;

    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("real")).unwrap();
    symlink(workspace.join("real"), workspace.join("link")).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\n[stack.app.mounts.link]\nsource = 'link'\ndestination = '/repo'\n"
        ),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(
        runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .is_err()
    );
}

#[test]
fn manifest_stack_task_plan_reuses_declared_binds_and_container_workdir() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("project")).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(
        workspace.join("bosn.toml"),
        format!(
            "[stack.app]\nimage = '{image}'\nworkdir = '/repo/project'\n[stack.app.mounts.repo]\nsource = '.'\ndestination = '/repo'\n[task.check]\nstack = 'app'\ncmd = 'pwd'\n"
        ),
    )
    .unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ManifestRuntimePlan { plan, .. } = runtime
        .run(manifest_stack_task_setup_plan(&ManifestAppTaskJobRequest {
            workspace,
            manifest: "bosn.toml".into(),
            stack: "app".into(),
            task_name: "check".into(),
            deadline: Duration::from_secs(1),
            output_limit: 64,
        }))
        .unwrap();
    assert_eq!(plan.app.workdir.as_deref(), Some("project"));
    assert_eq!(plan.app.mounts[0].target, "/repo");
    assert_eq!(plan.tasks["check"].command, "pwd");
}

#[test]
fn manifest_app_task_plan_retains_only_a_task_from_its_selected_stack() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    std::fs::write(workspace.join("bosn.toml"), format!(
        "[stack.app]\nimage='{image}'\n[stack.other]\nimage='{image}'\n[task.check]\nstack='app'\ncmd='printf ok'\n[task.foreign]\nstack='other'\ncmd='printf no'\n"
    )).unwrap();
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let request = ManifestAppTaskJobRequest {
        workspace: workspace.clone(),
        manifest: "bosn.toml".into(),
        stack: "app".into(),
        task_name: "check".into(),
        deadline: Duration::from_secs(1),
        output_limit: 64,
    };
    let ManifestRuntimePlan { plan, .. } = runtime
        .run(manifest_stack_task_setup_plan(&request))
        .unwrap();
    assert_eq!(plan.task_names, ["check"]);
    assert_eq!(plan.tasks["check"].command, "printf ok");
    let foreign = ManifestAppTaskJobRequest {
        task_name: "foreign".into(),
        ..request
    };
    assert!(
        runtime
            .run(manifest_stack_task_setup_plan(&foreign))
            .is_err()
    );
}

/// Two checkouts with the same `bosn.toml` must never share a setup container
/// that mounts one of them (#359); a stack that mounts no workspace path may.
#[test]
fn the_generation_names_the_workspace_it_mounts() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let image = format!("example.invalid/app@sha256:{}", "a".repeat(64));
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let generation = |clone: &str, manifest: &str| {
        let workspace = temporary.path().join(clone);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("bosn.toml"), manifest).unwrap();
        runtime
            .run(manifest_stack_setup_plan(&ManifestEnsureJobRequest {
                workspace,
                manifest: "bosn.toml".into(),
                stack: "app".into(),
                deadline: Duration::from_secs(1),
                output_limit: 64,
            }))
            .unwrap()
            .generation
    };
    let mounting = format!(
        "[stack.app]\nimage = '{image}'\n[stack.app.mounts.source]\nsource = '.'\ndestination = '/workspace'\n"
    );
    let first = generation("clone-a", &mounting);
    assert_ne!(
        first,
        generation("clone-b", &mounting),
        "each checkout gets its own container"
    );
    assert_eq!(
        first,
        generation("clone-a", &mounting),
        "stable for one checkout"
    );
    let detached = format!("[stack.app]\nimage = '{image}'\n");
    assert_eq!(
        generation("clone-c", &detached),
        generation("clone-d", &detached),
        "a stack that mounts no workspace path does not depend on the checkout"
    );
}
