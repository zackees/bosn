//! Daemon lifecycle, framing, peer checks, endpoint identity and doctor.

use super::*;

#[test]
fn second_daemon_is_refused_while_first_holds_writer() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let first = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let second = Service::new(state.clone()).serve().await;
        assert!(matches!(
            second,
            Err(Error::Registry(bosn_registry::Error::WriterAlreadyHeld(_)))
        ));
        client.shutdown().await.unwrap();
        stopped(first).await;
    });
}

#[test]
fn status_actor_serves_concurrent_typed_requests_before_clean_stop() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let mut requests = async_engine::TaskGroup::new();
        for _ in 0..8 {
            let client = client.clone();
            requests.spawn(async move { client.status().await });
        }
        while let Some(result) = requests.join_next().await {
            let status = result.unwrap().unwrap();
            assert_eq!(status.schema_version, 5);
        }
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn daemon_registry_diagnostics_are_bounded_safe_and_read_only() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let db = state.join("registry.sqlite3");
    let mut registry =
        Registry::create_writer(&db, "00000000-0000-4000-8000-000000000123").unwrap();
    let mut tx = registry.begin_immediate().unwrap();
    tx.put_resource(&Resource {
        id: "managed".into(),
        kind: ResourceKind::Container,
        name: "managed-app".into(),
        stack: "setup".into(),
        generation: "sha256:managed".into(),
        scope: Scope::Machine,
        workspace: "/private/workspace".into(),
        created_at: 1.0,
        last_used: 2.0,
        state: ResourceState::Active,
        retention: Retention::Pinned,
    })
    .unwrap();
    tx.append_event(1.0, "unrelated", "not exposed").unwrap();
    tx.append_event(2.0, "setup.ensure.succeeded", "job_id=1 outcome=succeeded")
        .unwrap();
    tx.commit().unwrap();
    drop(registry);
    let before = std::fs::metadata(&db).unwrap().len();
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let resources = client.registry_resources(0, 1).await.unwrap();
        assert_eq!(resources.records.len(), 1);
        assert_eq!(resources.records[0].id, "managed");
        assert_eq!(resources.records[0].name, "managed-app");
        assert!(!format!("{:?}", resources.records[0]).contains("/private/workspace"));
        let events = client.setup_ensure_events(0, 1).await.unwrap();
        assert_eq!(events.records[0].kind, "setup.ensure.succeeded");
        assert!(!events.records.iter().any(|event| event.kind == "unrelated"));
        assert!(matches!(
            client.registry_resources(0, 0).await,
            Err(Error::Protocol("invalid registry page limit"))
        ));
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
    assert_eq!(std::fs::metadata(&db).unwrap().len(), before);
}

#[test]
fn response_envelope_rejects_wrong_correlation_protocol_kind_and_encoding() {
    let mut payload = Vec::new();
    ReplyWire {
        code: 10,
        ..Default::default()
    }
    .encode(&mut payload)
    .unwrap();
    let request = DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new()).with_request_id(7);
    assert!(matches!(
        decode_response_frame(DaemonFrame::response_to(&request, payload.clone()), 7),
        Ok(Reply::Pong(version)) if version.is_empty()
    ));
    let mut versioned = Vec::new();
    ReplyWire {
        code: 10,
        daemon_version: "0.1.6".into(),
        ..Default::default()
    }
    .encode(&mut versioned)
    .unwrap();
    assert!(matches!(
        decode_response_frame(DaemonFrame::response_to(&request, versioned), 7),
        Ok(Reply::Pong(version)) if version == "0.1.6"
    ));
    let state = Path::new("/state");
    assert_eq!(daemon_version_mismatch(state, "0.1.6", "0.1.6"), None);
    let newer = daemon_version_mismatch(state, "0.1.6", "0.1.7").unwrap();
    assert!(newer.contains("is bosn 0.1.7, but this client is bosn 0.1.6"));
    assert!(newer.contains("`bosn daemon stop --state-dir /state`"));
    let legacy = daemon_version_mismatch(state, "0.1.6", "").unwrap();
    assert!(legacy.contains("0.1.5 or earlier"));
    for frame in [
        DaemonFrame::response_to(&request, payload.clone()).with_request_id(8),
        DaemonFrame::request(PAYLOAD_PROTOCOL, payload.clone()).with_request_id(7),
        DaemonFrame::response_to(&request, payload.clone()).with_raw_payload_encoding(1),
        DaemonFrame::response_to(
            &DaemonFrame::request(PAYLOAD_PROTOCOL + 1, Vec::new()),
            payload,
        ),
    ] {
        assert!(matches!(
            decode_response_frame(frame, 7),
            Err(Error::Protocol("response frame"))
        ));
    }
}

#[test]
fn peer_authorization_fails_closed_for_empty_or_other_user() {
    assert!(peer_is_authorized("current-user", "current-user"));
    assert!(!peer_is_authorized("", "current-user"));
    assert!(!peer_is_authorized("other-user", "current-user"));
}

#[test]
fn unsupported_request_protocol_returns_typed_error_response() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let mut payload = Vec::new();
        Request {
            protocol_version: PROTOCOL_VERSION + 1,
            operation: 1,
            workspace: String::new(),
            stack: String::new(),
            digest: String::new(),
            job_id: 0,
            log_after: 0,
            log_limit: 0,
            setup_config: String::new(),
            setup_policy: 0,
            setup_deadline_ms: 0,
            setup_output_limit: 0,
            setup_task_name: String::new(),
            diagnostic_after: 0,
            diagnostic_limit: 0,
            gc_candidate_token: String::new(),
            gc_confirm: false,
            setup_done_confirm: false,
            setup_adopt_confirm: false,
            unmanaged_include: Vec::new(),
            unmanaged_ttl_seconds: 0,
            ci_request: String::new(),
            follow_lease_ms: 0,
        }
        .encode(&mut payload)
        .unwrap();
        let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
            .await
            .unwrap();
        write_frame(
            &mut stream,
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(42),
        )
        .await
        .unwrap();
        let response = read_frame(&mut stream).await.unwrap();
        assert_eq!(response.request_id(), 42);
        assert!(matches!(
            decode_response_frame(response, 42),
            Err(Error::Protocol("unsupported protocol"))
        ));
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn malformed_oversized_and_stalled_clients_do_not_block_a_healthy_client() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;

        // Each bad peer is handled independently and dropped.  In
        // particular, partial input has an absolute per-frame deadline;
        // it cannot keep resetting a deadline by dribbling bytes.
        send_raw(&state, vec![0xff, 0xff]).await;
        send_raw(
            &state,
            DaemonFrameCodec::encode(
                &DaemonFrame::request(PAYLOAD_PROTOCOL, Vec::new()).with_raw_payload_encoding(1),
            )
            .unwrap(),
        )
        .await;
        send_raw(
            &state,
            DaemonFrameCodec::encode(&DaemonFrame::request(PAYLOAD_PROTOCOL, vec![0; MAX_FRAME]))
                .unwrap(),
        )
        .await;
        let _stalled = AsyncStream::connect(&endpoint(&state).unwrap())
            .await
            .unwrap();

        async_engine::timeout(Duration::from_millis(500), client.ping())
            .await
            .expect("stalled peer blocked ping")
            .unwrap();
        async_engine::timeout(Duration::from_millis(500), client.status())
            .await
            .expect("stalled peer blocked status")
            .unwrap();
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn slow_drip_frame_has_one_absolute_deadline() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let bytes =
            DaemonFrameCodec::encode(&DaemonFrame::request(PAYLOAD_PROTOCOL, vec![0; 64])).unwrap();
        let mut stream = AsyncStream::connect(&endpoint(&state).unwrap())
            .await
            .unwrap();
        for byte in bytes.iter().take(5) {
            let _ = stream.write_all(&[*byte]).await;
            async_engine::sleep(Duration::from_millis(800)).await;
        }
        // More than IO_DEADLINE elapsed since the first byte. A per-chunk
        // timeout would retain this client; the absolute deadline releases it.
        async_engine::timeout(Duration::from_millis(500), client.ping())
            .await
            .expect("slow drip blocked ping")
            .unwrap();
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn existing_database_aliases_share_endpoint_identity() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let alias = state.join(".");
        let alias_client = Client::for_state(&alias).unwrap();
        assert_eq!(
            alias_client.status().await.unwrap().registry_id,
            client.status().await.unwrap().registry_id
        );
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn a_short_state_dir_keeps_its_socket_beside_the_registry() {
    let state = Path::new("/var/lib/bosn-state");
    let ep = endpoint(state).unwrap();
    if kernal_api::platform::ipc::endpoint_is_filesystem_backed() {
        assert_eq!(Path::new(ep.display()), state.join("bosn-rs.sock"));
    }
}

#[cfg(unix)]
#[test]
fn a_state_dir_too_long_for_sun_path_still_serves() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary
        .path()
        .join("a-state-directory-name-long-enough-to-matter".repeat(3))
        .join("state");
    assert!(state.join("bosn-rs.sock").as_os_str().len() > 108);
    let ep = endpoint(&state).unwrap();
    assert!(
        ep.display().len() < ipc::endpoint_name_limit().max_bytes,
        "{}",
        ep.display()
    );
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone()).serve());
        let client = wait_for_client(&state).await;
        let ep = endpoint(&state).unwrap();
        assert!(
            ep.display().len() < ipc::endpoint_name_limit().max_bytes,
            "{}",
            ep.display()
        );
        assert!(ep.target_exists().unwrap());
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn regular_preexisting_endpoint_is_preserved_and_writer_is_released() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    // Create the database first so this uses the same inode-keyed endpoint
    // the service would select after acquiring its writer.
    std::fs::create_dir_all(&state).unwrap();
    let db = state.join("registry.sqlite3");
    let registry = Registry::create_writer(&db, "00000000-0000-4000-8000-000000000001").unwrap();
    drop(registry);
    let ep = endpoint(&state).unwrap();
    std::fs::write(ep.display(), b"do not remove").unwrap();

    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        assert!(matches!(
            Service::new(state.clone()).serve().await,
            Err(Error::EndpointOccupied(_))
        ));
    });
    assert_eq!(std::fs::read(ep.display()).unwrap(), b"do not remove");
    drop(Registry::open_writer(&db).expect("failed startup retained writer"));
}

#[test]
fn legacy_or_reconciliation_gated_registry_refuses_before_listening_or_mutation() {
    for reconciliation_required in [false, true] {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let db = state.join("registry.sqlite3");
        if reconciliation_required {
            let registry =
                Registry::create_writer(&db, "00000000-0000-4000-8000-000000000002").unwrap();
            drop(registry);
            let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
            connection.execute("INSERT INTO meta(key,value) VALUES('migration.reconciliation_required','true')", &[]).unwrap();
        } else {
            let connection = kernal_api::sqlite::Connection::open(&db).unwrap();
            connection
                .execute(
                    "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                    &[],
                )
                .unwrap();
            connection
                .execute("INSERT INTO meta VALUES ('schema_version','4')", &[])
                .unwrap();
        }
        let before = std::fs::read(&db).unwrap();
        let ep = endpoint(&state).unwrap();
        let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
        runtime.run(async {
            let result = Service::new(state.clone()).serve().await;
            if reconciliation_required {
                assert!(matches!(
                    result,
                    Err(Error::Registry(
                        bosn_registry::Error::ReconciliationRequired
                    ))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(Error::Registry(bosn_registry::Error::LegacyImportRequired(
                        4
                    )))
                ));
            }
        });
        assert!(!ep.target_exists().unwrap());
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }
}

#[test]
fn doctor_is_daemon_owned_read_only_and_uses_a_typed_fake_engine() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let fake = Arc::new(FakeDoctorExecutor::ready());
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(
            Service::new(state.clone())
                .with_doctor_executor(fake.clone())
                .serve(),
        );
        let client = wait_for_client(&state).await;
        let database = state.join("registry.sqlite3");
        let before = std::fs::read(&database).unwrap();
        let report = client.doctor().await.unwrap();
        assert_eq!(report.daemon, "ready");
        assert_eq!(report.registry, "ready");
        assert_eq!(report.engine, "ready");
        assert_eq!(report.client_version.as_deref(), Some("29.0.1"));
        assert_eq!(report.server_version.as_deref(), Some("29.0.1"));
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn doctor_wire_rejects_every_caller_control() {
    let request = || Request::operation(13);
    assert!(validate_doctor_request_wire(&request()).is_ok());
    for invalid in [
        Request {
            workspace: "/path".into(),
            ..request()
        },
        Request {
            setup_config: "https://user:secret@example.invalid/a".into(),
            ..request()
        },
        Request {
            setup_deadline_ms: 1,
            ..request()
        },
        Request {
            setup_output_limit: 1,
            ..request()
        },
        Request {
            diagnostic_limit: 1,
            ..request()
        },
        Request {
            job_id: 1,
            ..request()
        },
    ] {
        assert!(validate_doctor_request_wire(&invalid).is_err());
    }
}

#[test]
fn doctor_missing_daemon_is_typed_and_does_not_create_state() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("missing-state");
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .run(Client::for_state(&state).unwrap().doctor())
        .unwrap();
    assert_eq!(report.daemon, "unavailable");
    assert_eq!(report.registry, "unavailable");
    assert_eq!(report.engine, "unavailable");
    assert!(!state.exists());
}

#[test]
fn doctor_executor_deadline_is_typed_without_waiting_for_a_slow_engine() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let server = async_engine::launch(
            Service::new(state.clone())
                .with_doctor_executor(Arc::new(SlowDoctorExecutor))
                .serve(),
        );
        let client = wait_for_client(&state).await;
        let started = std::time::Instant::now();
        let report = client.doctor().await.unwrap();
        assert_eq!(report.daemon, "ready");
        assert_eq!(report.registry, "ready");
        assert_eq!(report.engine, "deadline");
        assert!(started.elapsed() < Duration::from_secs(3));
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}

#[test]
fn independent_state_directories_serve_concurrently() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let left = temporary.path().join("left");
    let right = temporary.path().join("right");
    let runtime = RuntimeBuilder::multi_thread().enable_all().build().unwrap();
    runtime.run(async {
        let left_server = async_engine::launch(Service::new(left.clone()).serve());
        let right_server = async_engine::launch(Service::new(right.clone()).serve());
        let left_client = wait_for_client(&left).await;
        let right_client = wait_for_client(&right).await;
        assert_ne!(
            left_client.status().await.unwrap().registry_id,
            right_client.status().await.unwrap().registry_id
        );
        left_client.shutdown().await.unwrap();
        right_client.shutdown().await.unwrap();
        stopped(left_server).await;
        stopped(right_server).await;
    });
}
