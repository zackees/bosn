use super::*;
#[cfg(feature = "embedded-python-tests")]
use bosn_service::{
    JobLogSink, Service, SetupEnsureExecution, SetupEnsureExecutor, SetupEnsureImageResource,
    SetupEnsureResource, SetupPrepareExecutor, SetupTaskExecutor,
};
#[cfg(feature = "embedded-python-tests")]
use kernal_api::async_engine::{self, CancellationToken, RuntimeBuilder};
#[cfg(feature = "embedded-python-tests")]
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

// A promptness fixture must own its interpreter: other parallel libtest
// cases can hold the process-wide GIL while its Python thread starts.
#[cfg(feature = "embedded-python-tests")]
fn isolated_python_fixture(test: &str) -> bool {
    const CHILD: &str = "BOSN_PYTHON_SUBMIT_TEST_PARENT";
    if let Some(parent) = std::env::var_os(CHILD) {
        assert_ne!(
            parent,
            std::ffi::OsString::from(std::process::id().to_string())
        );
        return false;
    }
    let output = kernal_api::run_bounded_command(
        kernal_api::SpawnSpec::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", test])
            .env(CHILD, std::process::id().to_string()),
        Duration::from_secs(10),
        65536,
    )
    .expect("isolated Python submission fixture must finish");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.exit.is_success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
    true
}

#[cfg(feature = "embedded-python-tests")]
struct FakeSetupExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
}

#[cfg(feature = "embedded-python-tests")]
impl FakeSetupExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "embedded-python-tests")]
impl SetupPrepareExecutor for FakeSetupExecutor {
    fn execute<'a>(
        &'a self,
        _request: SetupPrepareRequest,
        cancellation: &'a CancellationToken,
        logs: &'a JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            logs.send("[fake] setup preparation started".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            for _ in 0..100 {
                if cancellation.is_cancelled() {
                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                    return Err("fake cancellation observed".into());
                }
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            Ok("fake prepared sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into())
        })
    }
}

#[cfg(feature = "embedded-python-tests")]
struct FakeSetupTaskExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
}

#[cfg(feature = "embedded-python-tests")]
impl FakeSetupTaskExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "embedded-python-tests")]
impl SetupTaskExecutor for FakeSetupTaskExecutor {
    fn execute<'a>(
        &'a self,
        _request: SetupTaskJobRequest,
        cancellation: &'a CancellationToken,
        logs: &'a JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            logs.send("[fake] setup task started".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            for _ in 0..100 {
                if cancellation.is_cancelled() {
                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                    return Err("fake task cancellation observed".into());
                }
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            Ok("fake task completed".into())
        })
    }
}

#[cfg(feature = "embedded-python-tests")]
struct FakeSetupEnsureExecutor {
    started: AtomicUsize,
    cancelled: AtomicUsize,
}

#[cfg(feature = "embedded-python-tests")]
impl FakeSetupEnsureExecutor {
    fn new() -> Self {
        Self {
            started: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
        }
    }
}

#[cfg(feature = "embedded-python-tests")]
impl SetupEnsureExecutor for FakeSetupEnsureExecutor {
    fn execute<'a>(
        &'a self,
        request: SetupEnsureJobRequest,
        cancellation: &'a CancellationToken,
        logs: &'a JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            self.started.fetch_add(1, Ordering::SeqCst);
            logs.send("[fake] setup ensure started".into())
                .await
                .map_err(|_| "fake log consumer closed".to_owned())?;
            for _ in 0..100 {
                if cancellation.is_cancelled() {
                    self.cancelled.fetch_add(1, Ordering::SeqCst);
                    return Err("fake ensure cancellation observed".into());
                }
                async_engine::sleep(Duration::from_millis(10)).await;
            }
            Ok(SetupEnsureExecution {
                receipt: "fake setup ensured".into(),
                resource: SetupEnsureResource {
                    id: "setup-container:python-test".into(),
                    name: "bosn-setup-python-test".into(),
                    stack: "setup".into(),
                    generation: "sha256:python-test".into(),
                    workspace: request.workspace.to_string_lossy().into_owned(),
                },
                image: SetupEnsureImageResource {
                    id: "setup-image:sha256:python-test".into(),
                    name: "setup-image:sha256:python-test".into(),
                    stack: "setup".into(),
                    generation: "sha256:python-test".into(),
                    workspace: request.workspace.to_string_lossy().into_owned(),
                },
                volumes: Vec::new(),
                // Generic setup documents cannot opt into manifest
                // startup behavior; this fake mirrors production.
                manifest_autostart: false,
            })
        })
    }
}

#[test]
fn compose_plan_is_pure_and_returns_an_immutable_receipt() {
    let plan = plan_compose_yaml("services:\n  api:\n    image: alpine:3.21\n").unwrap();
    assert_eq!(plan.version, 1);
    assert!(plan.digest.starts_with("sha256:"));
    assert!(plan.document_json.contains("alpine:3.21"));
    assert!(plan.normalized_json.contains("alpine:3.21"));
    assert!(!plan.applied);
    assert!(plan_compose_yaml("services: {}\n").is_err());
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_module_exposes_compose_plan_without_a_client_or_daemon() {
    Python::initialize();
    Python::attach(|py| {
        let module = PyModule::new(py, "bosn_native_test").unwrap();
        _native(&module).unwrap();
        let plan = module
            .getattr("plan_compose_yaml")
            .unwrap()
            .call1(("services:\n  api:\n    image: alpine:3.21\n",))
            .unwrap();
        assert_eq!(
            plan.getattr("version").unwrap().extract::<u32>().unwrap(),
            1
        );
        assert!(!plan.getattr("applied").unwrap().extract::<bool>().unwrap());
        assert!(
            plan.getattr("document_json")
                .unwrap()
                .extract::<String>()
                .unwrap()
                .contains("alpine:3.21")
        );
    });
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_client_submits_and_observes_fake_setup_job_without_docker() {
    if isolated_python_fixture(
        "tests::python_client_submits_and_observes_fake_setup_job_without_docker",
    ) {
        return;
    }
    Python::initialize();
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executor = Arc::new(FakeSetupExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_prepare_executor(executor.clone())
                    .serve(),
            );
            let wire_client = wait_for_client(&state).await;
            let submitted = Instant::now();
            let python_state = state.clone();
            let python_workspace = workspace.clone();
            let (first, second) = std::thread::spawn(move || {
                Python::initialize();
                Python::attach(|py| {
                    let python_client = Client {
                        state_dir: python_state,
                    };
                    let first = python_client.submit_setup_prepare(
                        python_workspace.clone(),
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    let second = python_client.submit_setup_prepare(
                        python_workspace,
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    Ok::<_, PyErr>((first, second))
                })
            })
            .join()
            .expect("Python submit thread panicked")
            .unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(first, second);

            wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
            let page = wait_for_python_logs(&state, first).await;
            assert_eq!(page.records.len(), 2);
            assert_eq!(page.records[0].cursor, 0);
            assert!(page.records[0].line.starts_with("[bosn] raw output: "));
            assert_eq!(page.records[1].line, "[fake] setup preparation started");
            assert_eq!(page.next, 2);
            assert!(!page.gap);

            let python_state = state.clone();
            let running = std::thread::spawn(move || {
                Python::attach(|py| {
                    Client {
                        state_dir: python_state,
                    }
                    .job_status(first, py)
                })
            })
            .join()
            .expect("Python status thread panicked")
            .unwrap();
            assert_eq!(running.id, first);
            assert!(matches!(running.state.as_str(), "Running" | "Cancelling"));
            let python_state = state.clone();
            std::thread::spawn(move || {
                Python::attach(|py| {
                    Client {
                        state_dir: python_state,
                    }
                    .cancel_job(first, py)
                })
            })
            .join()
            .expect("Python cancellation thread panicked")
            .unwrap();
            wait_for_job_state(&wire_client, first, "Cancelled").await;
            assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

            wire_client.shutdown().await.unwrap();
            async_engine::timeout(Duration::from_secs(5), server)
                .await
                .expect("service did not stop")
                .expect("service task failed")
                .expect("service returned error");
        });
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_client_submits_and_cancels_coalesced_fake_setup_task_without_docker() {
    if isolated_python_fixture(
        "tests::python_client_submits_and_cancels_coalesced_fake_setup_task_without_docker",
    ) {
        return;
    }
    Python::initialize();
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executor = Arc::new(FakeSetupTaskExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_task_executor(executor.clone())
                    .serve(),
            );
            let wire_client = wait_for_client(&state).await;
            let submitted = Instant::now();
            let python_state = state.clone();
            let python_workspace = workspace.clone();
            let (first, second) = std::thread::spawn(move || {
                Python::initialize();
                Python::attach(|py| {
                    let python_client = Client {
                        state_dir: python_state,
                    };
                    let first = python_client.submit_setup_task(
                        python_workspace.clone(),
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        "wait".into(),
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    let second = python_client.submit_setup_task(
                        python_workspace,
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        "wait".into(),
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    Ok::<_, PyErr>((first, second))
                })
            })
            .join()
            .expect("Python submit thread panicked")
            .unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(first, second);

            wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
            let python_state = state.clone();
            let running = std::thread::spawn(move || {
                Python::attach(|py| {
                    Client {
                        state_dir: python_state,
                    }
                    .job_status(first, py)
                })
            })
            .join()
            .expect("Python status thread panicked")
            .unwrap();
            assert_eq!(running.id, first);
            assert!(matches!(running.state.as_str(), "Running" | "Cancelling"));

            let python_state = state.clone();
            std::thread::spawn(move || {
                Python::attach(|py| {
                    Client {
                        state_dir: python_state,
                    }
                    .cancel_job(first, py)
                })
            })
            .join()
            .expect("Python cancellation thread panicked")
            .unwrap();
            wait_for_job_state(&wire_client, first, "Cancelled").await;
            assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

            wire_client.shutdown().await.unwrap();
            async_engine::timeout(Duration::from_secs(5), server)
                .await
                .expect("service did not stop")
                .expect("service task failed")
                .expect("service returned error");
        });
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_client_submits_and_cancels_coalesced_fake_setup_ensure_without_docker() {
    if isolated_python_fixture(
        "tests::python_client_submits_and_cancels_coalesced_fake_setup_ensure_without_docker",
    ) {
        return;
    }
    Python::initialize();
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executor = Arc::new(FakeSetupEnsureExecutor::new());
    RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(
                Service::new(state.clone())
                    .with_setup_ensure_executor(executor.clone())
                    .serve(),
            );
            let wire_client = wait_for_client(&state).await;
            let submitted = Instant::now();
            let python_state = state.clone();
            let python_workspace = workspace.clone();
            let (first, second) = std::thread::spawn(move || {
                Python::initialize();
                Python::attach(|py| {
                    let python_client = Client {
                        state_dir: python_state,
                    };
                    let first = python_client.submit_setup_ensure(
                        python_workspace.clone(),
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    let second = python_client.submit_setup_ensure(
                        python_workspace,
                        "https://example.invalid/setup.toml".into(),
                        "online_refresh",
                        2_000,
                        4 * 1024,
                        py,
                    )?;
                    Ok::<_, PyErr>((first, second))
                })
            })
            .join()
            .expect("Python submit thread panicked")
            .unwrap();
            assert!(submitted.elapsed() < Duration::from_millis(250));
            assert_eq!(first, second);

            wait_for(|| executor.started.load(Ordering::SeqCst) == 1).await;
            let page = wait_for_python_logs(&state, first).await;
            assert!(page.records[0].line.starts_with("[bosn] raw output: "));
            assert_eq!(page.records[1].line, "[fake] setup ensure started");

            let python_state = state.clone();
            std::thread::spawn(move || {
                Python::attach(|py| {
                    Client {
                        state_dir: python_state,
                    }
                    .cancel_job(first, py)
                })
            })
            .join()
            .expect("Python cancellation thread panicked")
            .unwrap();
            wait_for_job_state(&wire_client, first, "Cancelled").await;
            assert_eq!(executor.cancelled.load(Ordering::SeqCst), 1);

            wire_client.shutdown().await.unwrap();
            async_engine::timeout(Duration::from_secs(5), server)
                .await
                .expect("service did not stop")
                .expect("service task failed")
                .expect("service returned error");
        });
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_prepare_input_and_diagnostics_do_not_expose_credentials() {
    Python::initialize();
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let client = Client {
        state_dir: temporary.path().join("no-daemon"),
    };
    Python::attach(|py| {
        let error = client
            .submit_setup_prepare(
                temporary.path().join("workspace"),
                "https://user:top-secret@example.invalid/setup.toml".into(),
                "online_refresh",
                1_000,
                4 * 1024,
                py,
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyValueError>(py));

        let error = client
            .submit_setup_ensure(
                temporary.path().join("workspace"),
                "https://user:ensure-secret@example.invalid/setup.toml".into(),
                "online_refresh",
                1_000,
                4 * 1024,
                py,
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyValueError>(py));
        assert!(!error.to_string().contains("ensure-secret"));

        let error = client
            .submit_setup_ensure(
                temporary.path().join("workspace"),
                "https://example.invalid/setup.toml".into(),
                "online_refresh",
                0,
                4 * 1024,
                py,
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyValueError>(py));
        assert!(!error.to_string().contains("top-secret"));

        let error = client
            .submit_setup_prepare(
                temporary.path().join("workspace"),
                "https://example.invalid/setup.toml".into(),
                "online_refresh",
                0,
                4 * 1024,
                py,
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyValueError>(py));

        let error = client.job_status(1, py).unwrap_err();
        assert!(error.is_instance_of::<PyRuntimeError>(py));
        assert!(!error.to_string().contains("no-daemon"));

        let error = client
            .setup_reconcile_repair_missing(
                temporary.path().join("workspace"),
                "srm1-00".into(),
                false,
                py,
            )
            .unwrap_err();
        assert!(error.is_instance_of::<PyValueError>(py));
    });
    assert_eq!(
        redact_diagnostic("https://user:secret@example.test/a?token=also-secret&safe=value"),
        "https://[redacted]@example.test/a?token=[redacted]&safe=value"
    );
}

#[cfg(feature = "embedded-python-tests")]
#[test]
fn python_registry_diagnostics_match_the_authenticated_daemon_surface() {
    Python::initialize();
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");

    RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .run(async {
            let server = async_engine::launch(Service::new(state.clone()).serve());
            let wire = wait_for_client(&state).await;
            let python_state = state.clone();
            let workspace = temporary.path().join("workspace");
            let (doctor, resources, events, preview) = std::thread::spawn(move || {
                Python::attach(|py| {
                    let client = Client {
                        state_dir: python_state,
                    };
                    Ok::<_, PyErr>((
                        client.doctor(py)?,
                        client.registry_resources(0, 1, py)?,
                        client.setup_ensure_events(0, 1, py)?,
                        client.setup_gc_preview(workspace, 0, 1, py)?,
                    ))
                })
            })
            .join()
            .unwrap()
            .unwrap();
            assert_eq!(doctor.daemon, "ready");
            assert_eq!(doctor.registry, "ready");
            assert!(resources.records.is_empty());
            assert!(events.records.is_empty());
            assert!(preview.next.is_none());
            let direct = wire.registry_resources(0, 1).await.unwrap();
            assert_eq!(direct.records.len(), resources.records.len());
            wire.shutdown().await.unwrap();
            async_engine::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        });
}

#[cfg(feature = "embedded-python-tests")]
async fn wait_for_client(state: &Path) -> ServiceClient {
    let client = ServiceClient::for_state(state).unwrap();
    // Up to 5 s: a loaded machine (a parallel build) can take well over
    // 500 ms to bring the fake daemon up; the common case still polls fast.
    for _ in 0..500 {
        if client.ping().await.is_ok() {
            return client;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("fake daemon did not become ready");
}

#[cfg(feature = "embedded-python-tests")]
async fn wait_for(predicate: impl Fn() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition did not become true");
}

#[cfg(feature = "embedded-python-tests")]
async fn wait_for_job_state(client: &ServiceClient, id: u64, wanted: &str) {
    for _ in 0..100 {
        if client.job_status(id).await.unwrap().state == wanted {
            return;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not reach {wanted}");
}

#[cfg(feature = "embedded-python-tests")]
async fn wait_for_python_logs(state: &Path, id: u64) -> JobLogPage {
    for _ in 0..100 {
        let state = state.to_path_buf();
        let page = std::thread::spawn(move || {
            Python::attach(|py| Client { state_dir: state }.job_logs(id, 0, 16, py))
        })
        .join()
        .expect("Python logs thread panicked")
        .unwrap();
        if page.records.len() >= 2 {
            return page;
        }
        async_engine::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {id} did not emit logs");
}
