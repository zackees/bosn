//! `fresh` app tasks: one new container per execution.
//!
//! An ordinary app task runs through `docker exec` in the long-lived setup
//! app, so every file it writes outside a declared volume is still there for
//! the next task. That is the point for a build cache, and a trap for an
//! experiment that must start from the image every time (a task that rewrote
//! a tool in the image's own store broke every later run). A `fresh` task
//! instead runs `docker run --rm` from the app's exact image and runtime shape
//! (binds, named volumes, tmpfs, environment, working directory): the declared
//! volumes persist and nothing else does.
//!
//! The app itself is still ensured and ownership-proven first. It owns the
//! named volumes, and its presence keeps them leased while a fresh task uses
//! them. The fresh container carries no Bosn ownership labels; it is removed
//! when its task ends, and force-removed when the client ends without the
//! task's exit status.

use std::time::Duration;

use bosn_engine::{CommandError, EngineEvent, RunOptions};
use kernal_api::async_engine::{CancellationSource, Sender};

use crate::task::{
    SetupAppTaskCommand, SetupAppTaskEngine, SetupAppTaskRequest, SetupTaskError, SetupTaskResult,
    finish_app_task, start_app_task,
};

/// Name prefix of every fresh task container; the rest is the task token.
pub const FRESH_TASK_CONTAINER_PREFIX: &str = "bosn-task-";
const REMOVE_DEADLINE: Duration = Duration::from_secs(45);
const REMOVE_OUTPUT_LIMIT: usize = 16 * 1024;

/// The image and runtime-shape flags of a fresh task container. Built only
/// from a validated plan inside this crate; callers cannot supply its flags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupFreshShape {
    image_identity: String,
    args: Vec<String>,
}

impl SetupFreshShape {
    /// The shape flags followed by the image, ready for the task's shell.
    pub(crate) fn docker_args(&self) -> Vec<String> {
        let mut args = self.args.clone();
        args.push(self.image_identity.clone());
        args
    }
}

/// Execute one declared task in a new container from the setup app's image
/// and runtime shape (see the module docs). Validation, budgets and the
/// receipt are exactly those of [`crate::execute_setup_app_task`]. A client
/// that ends without the task's exit status is followed by a forced removal
/// of the container; only a confirmed removal becomes
/// [`SetupTaskError::RemoteStopped`].
pub async fn execute_setup_app_task_fresh<E: SetupAppTaskEngine>(
    engine: &E,
    request: SetupAppTaskRequest<'_>,
) -> Result<SetupTaskResult, SetupTaskError> {
    let start = start_app_task(&request)?;
    let derived = crate::ensure::derive_creation(
        request.plan,
        &request.workspace_root,
        request.prepared_image,
    )
    .map_err(|_| SetupTaskError::InvalidRequest("invalid container creation profile"))?;
    if derived.macos_guest.is_some() {
        return Err(SetupTaskError::InvalidRequest(
            "fresh tasks are not supported for macOS guests",
        ));
    }
    let shape = SetupFreshShape {
        image_identity: derived.image_identity.clone(),
        args: derived.runtime_shape_args(),
    };
    let container_name = format!("{FRESH_TASK_CONTAINER_PREFIX}{}", start.task_token);
    let result = match engine
        .stream(
            SetupAppTaskCommand::FreshRun {
                container_name: container_name.clone(),
                task_token: start.task_token.clone(),
                passthrough_env: request.passthrough_env.clone(),
                shape,
                command: start.command.clone(),
            },
            RunOptions::streaming(start.remaining, request.options.output_limit),
            request.cancellation,
            request.events,
        )
        .await
    {
        Ok(result) => result,
        // The client never started, so no container was created.
        Err(error @ CommandError::Spawn(_)) => return Err(error.into()),
        Err(error) => {
            let cause = SetupTaskError::from(error);
            let removed = remove_fresh_container(engine, &container_name, request.events).await;
            return Err(if removed {
                SetupTaskError::RemoteStopped(Box::new(cause))
            } else {
                cause
            });
        }
    };
    finish_app_task(result, request, start.image_identity)
}

/// Force-remove one fresh task container under its own budget: the job's
/// cancellation has already fired by the time this is needed.
async fn remove_fresh_container<E: SetupAppTaskEngine>(
    engine: &E,
    container_name: &str,
    events: &Sender<EngineEvent>,
) -> bool {
    let independent = CancellationSource::new();
    let cancellation = independent.token();
    matches!(
        engine
            .stream(
                SetupAppTaskCommand::FreshRemove {
                    container_name: container_name.to_owned(),
                },
                RunOptions::streaming(REMOVE_DEADLINE, REMOVE_OUTPUT_LIMIT),
                &cancellation,
                events,
            )
            .await,
        Ok(result) if result.ok()
    )
}
