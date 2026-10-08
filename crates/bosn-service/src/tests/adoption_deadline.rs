//! Adoption's operation deadline must outlive the ordinary short RPC allowance.
use super::*;

struct SlowAdoption;
impl SetupAdoptExecutor for SlowAdoption {
    fn execute<'a>(
        &'a self,
        request: SetupAdoptRequest,
        _cancellation: &'a async_engine::CancellationToken,
        _logs: &'a crate::raw_run_log::JobLogSink,
    ) -> Pin<Box<dyn Future<Output = Result<SetupEnsureExecution, String>> + Send + 'a>> {
        Box::pin(async move {
            assert!(request.deadline <= Duration::from_secs(5));
            async_engine::sleep(IO_DEADLINE + Duration::from_millis(250)).await;
            Err("slow adoption reached ownership refusal".into())
        })
    }
}

#[test]
fn adoption_waits_for_operation_and_preserves_remote_failure_after_short_rpc_limit() {
    let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let state = temporary.path().join("state");
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.run(async {
        let server = async_engine::launch(Service::new(state.clone())
            .with_setup_adopt_executor(Arc::new(SlowAdoption)).serve());
        let client = wait_for_client(&state).await;
        let started = std::time::Instant::now();
        let error = client.setup_adopt(SetupAdoptRequest {
            workspace: temporary.path().to_path_buf(), config: "setup.toml".into(),
            policy: SetupPreparePolicy::Refresh, deadline: Duration::from_secs(5),
            output_limit: 4096, confirm: true,
        }).await.unwrap_err();
        assert!(matches!(error, Error::Remote(ref message) if message == "slow adoption reached ownership refusal"), "{error:?}");
        assert!(started.elapsed() > IO_DEADLINE);
        client.shutdown().await.unwrap();
        stopped(server).await;
    });
}
