//! Bounded CI Docker controls whose owned client stops on future cancellation.
use super::{CONTROL_OUTPUT, DockerActBackend, RunOptions};
use kernal_api::async_engine;
use std::time::Duration;

impl DockerActBackend {
    pub(super) async fn run(
        &self,
        args: Vec<String>,
        deadline: Duration,
    ) -> Result<bosn_engine::CommandResult, String> {
        self.run_bounded(args, RunOptions::bounded(deadline, CONTROL_OUTPUT))
            .await
    }

    pub(super) async fn run_bounded(
        &self,
        args: Vec<String>,
        options: RunOptions,
    ) -> Result<bosn_engine::CommandResult, String> {
        // A blocking capture survives future cancellation and holds runtime
        // teardown until its deadline. Sessions kill the owned client on drop;
        // remote Docker effects still require durable ownership reconciliation.
        let engine = self.docker.with_args(args);
        let (events, mut receiver) = async_engine::channel(64);
        let capture = async move { Box::pin(engine.stream(options, None, &events)).await };
        let drain = async { while receiver.recv().await.is_some() {} };
        let (result, ()) = async_engine::join(capture, drain).await;
        result.map_err(|error| error.to_string())
    }
}
