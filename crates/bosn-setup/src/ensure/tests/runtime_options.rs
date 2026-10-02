//! Typed tmpfs and host Docker socket options: emitted from typed fields, never colliding.

use super::*;

#[test]
fn typed_tmpfs_is_emitted_without_a_raw_option_channel() {
    let command = SetupEnsureCommand::Create {
        container_name: "bosn-setup-test".into(),
        image_identity: IDENTITY.into(),
        mounts: Vec::new(),
        volumes: Vec::new(),
        tmpfs: vec![SetupEnsureTmpfs {
            target: "/run/cache".into(),
            readonly: true,
            size: Some(crate::SetupTmpfsSize {
                value: 64,
                unit: crate::SetupTmpfsSizeUnit::Mebibytes,
            }),
            exec: None,
            mode: None,
        }],
        host_docker_socket: None,
        environment: BTreeMap::new(),
        workdir: None,
        command: None,
        labels: BTreeMap::new(),
        macos_guest: Box::new(None),
    };
    let args = command.docker_args();
    assert_eq!(
        args.windows(2)
            .find(|pair| pair[0] == "--tmpfs")
            .map(|pair| pair[1].as_str()),
        Some("/run/cache:ro,size=64m")
    );
}

#[test]
fn typed_tmpfs_exec_mode_and_host_docker_socket_are_emitted_from_typed_fields() {
    let command = SetupEnsureCommand::Create {
        container_name: "bosn-setup-test".into(),
        image_identity: IDENTITY.into(),
        mounts: Vec::new(),
        volumes: Vec::new(),
        tmpfs: vec![SetupEnsureTmpfs {
            target: "/mount-probe".into(),
            readonly: false,
            size: None,
            exec: Some(true),
            mode: Some(0o1777),
        }],
        host_docker_socket: Some(crate::SetupHostDockerSocket {
            source: crate::SetupHostDockerSocketSource::VarRun,
            target: "/var/run/docker.sock".into(),
            readonly: false,
        }),
        environment: BTreeMap::new(),
        workdir: None,
        command: None,
        labels: BTreeMap::new(),
        macos_guest: Box::new(None),
    };
    let args = command.docker_args();
    assert_eq!(
        args.windows(2)
            .find(|pair| pair[0] == "--tmpfs")
            .map(|pair| pair[1].as_str()),
        Some("/mount-probe:exec,mode=1777")
    );
    assert!(args.windows(2).any(|pair| pair[0] == "--mount"
        && pair[1] == "type=bind,src=/var/run/docker.sock,dst=/var/run/docker.sock"));
    let noexec = SetupEnsureTmpfs {
        target: "/t".into(),
        readonly: true,
        size: None,
        exec: Some(false),
        mode: Some(0o700),
    };
    assert_eq!(tmpfs_docker_value(&noexec), "/t:ro,noexec,mode=700");
}

#[test]
fn host_docker_socket_target_cannot_collide_or_enter_a_guest() {
    let temporary = tempfile::tempdir().unwrap();
    let mut plan = plan(temporary.path());
    plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
        source: crate::SetupHostDockerSocketSource::Run,
        target: plan.app.mounts[0].target.clone(),
        readonly: false,
    });
    assert!(matches!(
        validate_plan_shape(&plan),
        Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified"
        ))
    ));
    plan.host_docker_socket = Some(crate::SetupHostDockerSocket {
        source: crate::SetupHostDockerSocketSource::Run,
        target: "/var/run/docker.sock".into(),
        readonly: false,
    });
    assert!(validate_plan_shape(&plan).is_ok());

    let mut guest = macos_guest_plan(temporary.path());
    assert!(validate_plan_shape(&guest).is_ok());
    guest.host_docker_socket = plan.host_docker_socket.clone();
    assert!(matches!(
        validate_plan_shape(&guest),
        Err(SetupEnsureError::InvalidRequest(
            "host Docker socket receipt was modified"
        ))
    ));
}

#[test]
fn tmpfs_target_cannot_collide_with_a_bind_or_be_modified() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("src")).unwrap();
    let mut plan = plan(&workspace);
    plan.tmpfs.push(crate::SetupTmpfs {
        target: "/workspace".into(),
        readonly: false,
        size: None,
        exec: None,
        mode: None,
    });
    let image = prepared(&plan);
    let engine = FakeEngine::with_results([]);
    let cancellation = CancellationSource::new();
    assert!(matches!(
        run(
            &engine,
            &plan,
            &workspace,
            &image,
            &cancellation.token(),
            RunOptions::streaming(Duration::from_secs(2), 4096),
        ),
        Err(SetupEnsureError::InvalidRequest(
            "tmpfs receipt was modified"
        ))
    ));
    assert!(engine.calls.lock().unwrap().is_empty());
}
