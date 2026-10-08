//! Session recorders and request digests used by the job actor.

use super::*;

pub(crate) struct ActorSetupImageRecorder {
    pub(crate) actor: RegistryActor,
}

impl SetupImageRecorder for ActorSetupImageRecorder {
    fn record_preparation<'a>(
        &'a self,
        intent: bosn_registry::ImageCreationIntent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_image_creation_intent(intent, false)
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn complete_preparation<'a>(
        &'a self,
        intent: bosn_registry::ImageCreationIntent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_image_creation_intent(intent, true)
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn record_container_intent<'a>(
        &'a self,
        container: SetupEnsureResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_container_intent(container)
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn record<'a>(
        &'a self,
        image: SetupEnsureImageResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_prepared_image(image)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Compatibility for direct executor test calls outside daemon ownership.
pub(crate) struct ReceiptOnlyImageRecorder;
impl SetupImageRecorder for ReceiptOnlyImageRecorder {
    fn record<'a>(
        &'a self,
        _image: SetupEnsureImageResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

impl SetupJobRequest {
    pub(crate) fn kind(&self) -> SetupJobKind {
        match self {
            Self::Prepare(_) => SetupJobKind::Prepare,
            Self::Task(_) => SetupJobKind::Task,
            Self::AppTask(_) => SetupJobKind::AppTask,
            Self::Ensure(_) => SetupJobKind::Ensure,
            Self::ManifestEnsure(_) => SetupJobKind::ManifestEnsure,
            Self::ManifestConverge(_) => SetupJobKind::ManifestConverge,
            Self::ManifestAppTask(_) => SetupJobKind::ManifestAppTask,
        }
    }

    pub(crate) fn deadline_mut(&mut self) -> &mut Duration {
        match self {
            Self::Prepare(request) => &mut request.deadline,
            Self::Task(request) => &mut request.deadline,
            Self::AppTask(request) => &mut request.deadline,
            Self::Ensure(request) => &mut request.deadline,
            Self::ManifestEnsure(request) => &mut request.deadline,
            Self::ManifestConverge(request) => &mut request.deadline,
            Self::ManifestAppTask(request) => &mut request.deadline,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActorSetupAppTaskSessionRecorder {
    pub(crate) actor: RegistryActor,
    pub(crate) job_id: u64,
}

impl SetupAppTaskSessionRecorder for ActorSetupAppTaskSessionRecorder {
    fn checkpoint_preparation<'a>(
        &'a self,
        intent: bosn_registry::ImageCreationIntent,
        complete: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_image_creation_intent(intent, complete)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn record_image<'a>(
        &'a self,
        image: SetupEnsureImageResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_prepared_image(image)
                .await
                .map_err(|error| error.to_string())
        })
    }
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .begin_setup_app_task_session(self.job_id, managed_container_identity)
                .await
                .map_err(|_| "registry session start failed".into())
        })
    }
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .finish_setup_app_task_session(self.job_id, outcome)
                .await
                .map_err(|_| "registry session finish failed".into())
        })
    }
}

#[derive(Clone)]
pub(crate) struct ActorManifestAppTaskSessionRecorder {
    pub(crate) actor: RegistryActor,
    pub(crate) job_id: u64,
    pub(crate) run: Option<RunContext>,
}
impl ManifestAppTaskSessionRecorder for ActorManifestAppTaskSessionRecorder {
    fn checkpoint_preparation<'a>(
        &'a self,
        intent: bosn_registry::ImageCreationIntent,
        complete: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_image_creation_intent(intent, complete)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn record_image<'a>(
        &'a self,
        image: SetupEnsureImageResource,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .record_prepared_image(image)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn run_context(&self) -> Option<&RunContext> {
        self.run.as_ref()
    }
    fn stop_retired_generations<'a>(
        &'a self,
        workspace: &'a str,
        stack: &'a str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>> {
        Box::pin(async move {
            match stop_retired_stack_containers(&self.actor, workspace, stack).await {
                Ok(stops) => stops
                    .stopped
                    .iter()
                    .map(|name| format!("[manifest-app-task] stopped retired generation {name}"))
                    .chain(stops.failed.iter().map(|(name, error)| {
                        format!("[manifest-app-task] warning: retired generation {name} was not stopped: {error}")
                    }))
                    .collect(),
                Err(error) => vec![format!(
                    "[manifest-app-task] warning: retired generations of stack {stack} were not listed: {error}"
                )],
            }
        })
    }
    fn begin<'a>(
        &'a self,
        managed_container_identity: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .begin_manifest_app_task_session(self.job_id, managed_container_identity)
                .await
                .map_err(|_| "registry session start failed".into())
        })
    }
    fn finish<'a>(
        &'a self,
        outcome: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.actor
                .finish_manifest_app_task_session(self.job_id, outcome)
                .await
                .map_err(|_| "registry session finish failed".into())
        })
    }
}

pub(crate) fn bounded_log_line(value: &str) -> String {
    if value.len() <= jobs::MAX_LOG_LINE_BYTES {
        return value.to_owned();
    }
    let mut end = jobs::MAX_LOG_LINE_BYTES.saturating_sub(3);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = value[..end].to_owned();
    bounded.push_str("...");
    bounded
}

pub(crate) fn setup_prepare_digest(request: &SetupPrepareRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-prepare.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

pub(crate) fn setup_task_digest(request: &SetupTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-task.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        request.task_name.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

pub(crate) fn setup_app_task_digest(request: &SetupAppTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-app-task.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        request.task_name.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

pub(crate) fn setup_ensure_digest(request: &SetupEnsureJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let policy = request.policy.wire().to_le_bytes();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.setup-ensure.v1".as_slice(),
        workspace.as_bytes(),
        request.config.as_bytes(),
        &policy,
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "setup:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}

pub(crate) fn manifest_ensure_digest(request: &ManifestEnsureJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.manifest-ensure.v1".as_slice(),
        workspace.as_bytes(),
        request.manifest.as_bytes(),
        request.stack.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "manifest:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}
pub(crate) fn manifest_converge_digest(request: &ManifestConvergeJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.manifest-converge.v1".as_slice(),
        workspace.as_bytes(),
        request.manifest.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "manifest-converge:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}
pub(crate) fn manifest_app_task_digest(request: &ManifestAppTaskJobRequest) -> String {
    let mut material = Vec::new();
    let workspace = request.workspace.to_string_lossy();
    let deadline = request.deadline.as_millis().to_le_bytes();
    let output_limit = (request.output_limit as u64).to_le_bytes();
    for part in [
        b"bosn.manifest-app-task.v1".as_slice(),
        workspace.as_bytes(),
        request.manifest.as_bytes(),
        request.stack.as_bytes(),
        request.task_name.as_bytes(),
        &deadline,
        &output_limit,
    ] {
        material.extend_from_slice(&(part.len() as u64).to_le_bytes());
        material.extend_from_slice(part);
    }
    format!(
        "manifest:{}",
        kernal_api::hash::blake3_bytes(&material).to_hex()
    )
}
