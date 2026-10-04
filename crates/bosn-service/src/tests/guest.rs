//! macOS guest SSH tasks and payload uploads.

use super::*;

#[test]
fn guest_task_uses_only_daemon_identity_loopback_and_retains_uncertain_session() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let identity = state.join("guest-ssh").join("id_ed25519");
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, "not-a-real-key-for-unit-test").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &identity,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let transport = FakeGuestSshTransport::new([
        Ok(command_result(0, [])),
        // SSH exit 255 is intentionally ambiguous: the local client
        // cannot prove whether the remote command completed.
        Ok(command_result(255, "connection reset")),
    ]);
    let session = FakeManifestGuestSession::default();
    let observed = SetupEnsureResult {
        container_name: format!("bosn-setup-{TEST_HASH}"),
        container_id: TEST_CONTAINER_ID.into(),
        image_identity: TEST_IDENTITY.into(),
        created: false,
        started: false,
        running: true,
    };
    let (events, _receiver) = async_engine::channel(8);
    let (text_logs, _log_receiver) = async_engine::channel(8);
    let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let error = execute_manifest_guest_ssh_task(
                &transport,
                &state,
                &workspace,
                &observed,
                &ManifestGuestTask {
                    ssh_user: "runner".into(),
                    ssh_port: 2222,
                    workdir: Some("/Users/runner/space dir".into()),
                    command: "echo declared; true".into(),
                    payload: None,
                },
                "check",
                &async_engine::Deadline::after(Duration::from_secs(1)),
                1024,
                &token,
                &logs,
                &events,
                &session,
            )
            .await
            .unwrap_err();
            assert!(error.contains("completion is unknown"));
        });
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].command, "true");
    assert_eq!(calls[1].port, 2222);
    assert_eq!(calls[1].user, "runner");
    assert_eq!(calls[1].identity_file, identity);
    assert_eq!(
        calls[1].command,
        "cd '/Users/runner/space dir' && echo declared; true"
    );
    assert_eq!(
        session.events.lock().unwrap().as_slice(),
        [
            format!("begin:bosn-setup-{TEST_HASH}"),
            "finish:uncertain".into()
        ]
    );
}

#[test]
fn guest_task_cancellation_after_remote_start_is_uncertain() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let identity = state.join("guest-ssh").join("id_ed25519");
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, "unit-test-key").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &identity,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let transport = FakeGuestSshTransport::new([
        Ok(command_result(0, [])),
        Err(CommandError::Cancelled {
            reaped_pid: Some(1),
            cleanup: None,
        }),
    ]);
    let session = FakeManifestGuestSession::default();
    let observed = SetupEnsureResult {
        container_name: format!("bosn-setup-{TEST_HASH}"),
        container_id: TEST_CONTAINER_ID.into(),
        image_identity: TEST_IDENTITY.into(),
        created: false,
        started: false,
        running: true,
    };
    let (events, _receiver) = async_engine::channel(8);
    let (text_logs, _log_receiver) = async_engine::channel(8);
    let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            assert!(
                execute_manifest_guest_ssh_task(
                    &transport,
                    &state,
                    &workspace,
                    &observed,
                    &ManifestGuestTask {
                        ssh_user: "runner".into(),
                        ssh_port: 2222,
                        workdir: None,
                        command: "sleep 10".into(),
                        payload: None,
                    },
                    "wait",
                    &async_engine::Deadline::after(Duration::from_secs(1)),
                    1024,
                    &token,
                    &logs,
                    &events,
                    &session,
                )
                .await
                .unwrap_err()
                .contains("completion is unknown")
            );
        });
    assert_eq!(
        session.events.lock().unwrap().as_slice(),
        [
            format!("begin:bosn-setup-{TEST_HASH}"),
            "finish:uncertain".into()
        ]
    );
}

#[test]
fn guest_payload_is_copied_before_the_declared_task_with_no_raw_transport_inputs() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let identity = state.join("guest-ssh").join("id_ed25519");
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, "unit-test-key").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &identity,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let payload = workspace.join("out").join("archive.tar.zst");
    std::fs::create_dir(payload.parent().unwrap()).unwrap();
    std::fs::write(&payload, "archive").unwrap();
    let transport = FakeGuestSshTransport::new([
        Ok(command_result(0, [])),
        Ok(command_result(0, [])),
        Ok(command_result(0, [])),
    ]);
    let session = FakeManifestGuestSession::default();
    let observed = SetupEnsureResult {
        container_name: format!("bosn-setup-{TEST_HASH}"),
        container_id: TEST_CONTAINER_ID.into(),
        image_identity: TEST_IDENTITY.into(),
        created: false,
        started: false,
        running: true,
    };
    let (events, _receiver) = async_engine::channel(8);
    let (text_logs, _log_receiver) = async_engine::channel(8);
    let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            execute_manifest_guest_ssh_task(
                &transport,
                &state,
                &workspace,
                &observed,
                &ManifestGuestTask {
                    ssh_user: "runner".into(),
                    ssh_port: 2222,
                    workdir: None,
                    command: "run-declared-task".into(),
                    payload: Some(ManifestGuestPayload {
                        source: "out/archive.tar.zst".into(),
                        destination: "~/archive.tar.zst".into(),
                    }),
                },
                "check",
                &async_engine::Deadline::after(Duration::from_secs(1)),
                1024,
                &token,
                &logs,
                &events,
                &session,
            )
            .await
            .unwrap();
        });
    let ssh = transport.calls.lock().unwrap();
    assert_eq!(ssh.len(), 2);
    assert_eq!(ssh[0].command, "true");
    assert_eq!(ssh[1].command, "run-declared-task");
    let scp = transport.scp_calls.lock().unwrap();
    assert_eq!(scp.len(), 1);
    assert_eq!(scp[0].source, payload);
    assert_eq!(scp[0].destination, "~/archive.tar.zst");
    assert_eq!(scp[0].user, "runner");
    assert_eq!(scp[0].port, 2222);
    assert_eq!(scp[0].identity_file, identity);
    assert_eq!(
        transport.sequence.lock().unwrap().as_slice(),
        ["ssh:true", "scp:~/archive.tar.zst", "ssh:run-declared-task"]
    );
    assert_eq!(
        session.events.lock().unwrap().as_slice(),
        [
            format!("begin:bosn-setup-{TEST_HASH}"),
            "finish:succeeded".into()
        ]
    );
}

#[test]
fn failed_guest_payload_upload_never_starts_or_records_the_task() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let identity = state.join("guest-ssh").join("id_ed25519");
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, "unit-test-key").unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(
        &identity,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(workspace.join("archive.tar.zst"), "archive").unwrap();
    let transport = FakeGuestSshTransport::new([
        Ok(command_result(0, [])),
        Ok(command_result(1, "permission denied")),
    ]);
    let session = FakeManifestGuestSession::default();
    let observed = SetupEnsureResult {
        container_name: format!("bosn-setup-{TEST_HASH}"),
        container_id: TEST_CONTAINER_ID.into(),
        image_identity: TEST_IDENTITY.into(),
        created: false,
        started: false,
        running: true,
    };
    let (events, _receiver) = async_engine::channel(8);
    let (text_logs, _log_receiver) = async_engine::channel(8);
    let logs = crate::raw_run_log::JobLogSink::transient(text_logs);
    let cancellation = CancellationSource::new();
    let token = cancellation.token();
    let error = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            execute_manifest_guest_ssh_task(
                &transport,
                &state,
                &workspace,
                &observed,
                &ManifestGuestTask {
                    ssh_user: "runner".into(),
                    ssh_port: 2222,
                    workdir: None,
                    command: "must-not-run".into(),
                    payload: Some(ManifestGuestPayload {
                        source: "archive.tar.zst".into(),
                        destination: "/Users/runner/archive.tar.zst".into(),
                    }),
                },
                "check",
                &async_engine::Deadline::after(Duration::from_secs(1)),
                1024,
                &token,
                &logs,
                &events,
                &session,
            )
            .await
            .unwrap_err()
        });
    assert!(error.contains("SCP payload upload failed"));
    assert!(error.contains("declared task was not started"));
    assert_eq!(transport.calls.lock().unwrap().len(), 1);
    assert_eq!(transport.scp_calls.lock().unwrap().len(), 1);
    assert_eq!(
        transport.sequence.lock().unwrap().as_slice(),
        ["ssh:true", "scp:/Users/runner/archive.tar.zst"]
    );
    assert!(session.events.lock().unwrap().is_empty());
}

#[test]
fn guest_payload_preflight_requires_a_bounded_regular_workspace_file_and_normal_destination() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let payload = workspace.join("archive.tar.zst");
    std::fs::write(&payload, "archive").unwrap();
    assert_eq!(
        manifest_guest_payload_file(&workspace, "archive.tar.zst").unwrap(),
        payload
    );
    assert!(manifest_guest_payload_file(&workspace, "../outside").is_err());
    assert!(manifest_guest_payload_file(&workspace, ".").is_err());
    assert!(manifest_guest_payload_file(&workspace, "missing").is_err());
    let oversized = workspace.join("oversized");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(MAX_MANIFEST_GUEST_PAYLOAD_BYTES + 1)
        .unwrap();
    assert!(
        manifest_guest_payload_file(&workspace, "oversized")
            .unwrap_err()
            .contains("exceeds")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("archive.tar.zst", workspace.join("linked")).unwrap();
        assert!(
            manifest_guest_payload_file(&workspace, "linked")
                .unwrap_err()
                .contains("regular file")
        );
    }
    assert_eq!(
        normalize_manifest_guest_payload_destination("~/archive.tar.zst").unwrap(),
        "~/archive.tar.zst"
    );
    assert_eq!(
        normalize_manifest_guest_payload_destination("/Users/runner/archive.tar.zst").unwrap(),
        "/Users/runner/archive.tar.zst"
    );
    for invalid in ["relative", "~/dir/../file", "/tmp//file", "~/file:name"] {
        assert!(
            normalize_manifest_guest_payload_destination(invalid).is_err(),
            "{invalid}"
        );
    }
}
