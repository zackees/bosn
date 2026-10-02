//! Blocking daemon calls, each on its own short-lived kernal-api runtime.

use super::*;

pub(crate) fn status(state_dir: &Path) -> Result<ServiceStatus, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async { ServiceClient::for_state(state_dir)?.status().await })
}

pub(crate) fn doctor(state_dir: &Path) -> Result<ServiceDoctorReport, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async { ServiceClient::for_state(state_dir)?.doctor().await })
}

pub(crate) fn registry_resources(
    state_dir: &Path,
    after: u64,
    limit: u32,
) -> Result<ServiceRegistryResourcePage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .registry_resources(after, limit)
            .await
    })
}

pub(crate) fn setup_ensure_events(
    state_dir: &Path,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupEnsureEventPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_ensure_events(after, limit)
            .await
    })
}
pub(crate) fn setup_gc_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupGcPreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_gc_preview(workspace, after, limit)
            .await
    })
}
pub(crate) fn manifest_volume_gc_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceManifestVolumeGcPreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .manifest_volume_gc_preview(workspace, after, limit)
            .await
    })
}
pub(crate) fn manifest_volume_release_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceManifestVolumeGcPreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .manifest_volume_release_preview(workspace, after, limit)
            .await
    })
}
pub(crate) fn setup_reconcile_preview(
    state_dir: &Path,
    workspace: PathBuf,
    after: u64,
    limit: u32,
) -> Result<ServiceSetupReconcilePreviewPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_reconcile_preview(workspace, after, limit)
            .await
    })
}
pub(crate) fn setup_reconcile_repair_missing(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupReconcileMissingRepairResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_reconcile_repair_missing(workspace, &candidate_token, true)
            .await
    })
}
pub(crate) fn setup_gc_apply(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupGcApplyResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_gc_apply(workspace, &candidate_token, true)
            .await
    })
}
pub(crate) fn manifest_volume_gc_apply(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceManifestVolumeGcApplyResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .manifest_volume_gc_apply(workspace, &candidate_token, true)
            .await
    })
}
pub(crate) fn manifest_volume_release_apply(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceManifestVolumeGcApplyResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .manifest_volume_release_apply(workspace, &candidate_token, true)
            .await
    })
}
pub(crate) fn setup_stop_retired(
    state_dir: &Path,
    workspace: PathBuf,
    candidate_token: String,
) -> Result<ServiceSetupRetiredStopResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_stop_retired(workspace, &candidate_token, true)
            .await
    })
}

pub(crate) fn setup_done(
    state_dir: &Path,
    workspace: PathBuf,
) -> Result<ServiceSetupDoneResult, bosn_service::Error> {
    let client = ServiceClient::for_state(state_dir)?;
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(bosn_service::Error::Io)?;
    runtime.run(client.setup_done(workspace, true))
}
pub(crate) fn setup_adopt(
    state_dir: &Path,
    request: SetupAdoptRequest,
) -> Result<bosn_service::SetupAdoptResult, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .setup_adopt(request)
            .await
    })
}

pub(crate) fn submit_setup_prepare(
    state_dir: &Path,
    request: SetupPrepareRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_prepare(request)
            .await
    })
}

pub(crate) fn submit_setup_task(
    state_dir: &Path,
    request: SetupTaskJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_task(request)
            .await
    })
}

pub(crate) fn submit_setup_app_task(
    state_dir: &Path,
    request: SetupAppTaskJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_app_task(request)
            .await
    })
}

pub(crate) fn submit_setup_ensure(
    state_dir: &Path,
    request: SetupEnsureJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_setup_ensure(request)
            .await
    })
}

pub(crate) fn submit_manifest_ensure(
    state_dir: &Path,
    request: ManifestEnsureJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_manifest_ensure(request)
            .await
    })
}
pub(crate) fn submit_manifest_converge(
    state_dir: &Path,
    request: ManifestConvergeJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_manifest_converge(request)
            .await
    })
}
pub(crate) fn submit_manifest_app_task(
    state_dir: &Path,
    request: ManifestAppTaskJobRequest,
) -> Result<u64, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .submit_manifest_app_task(request)
            .await
    })
}

pub(crate) fn jobs_json(state_dir: &Path) -> Result<String, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .jobs()
            .await
            .map(|view| view.to_string())
    })
}

pub(crate) fn job_status(
    state_dir: &Path,
    job_id: u64,
) -> Result<ServiceJobStatus, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .job_status(job_id)
            .await
    })
}

pub(crate) fn job_logs(
    state_dir: &Path,
    job_id: u64,
    after: u64,
    limit: u32,
) -> Result<ServiceJobLogPage, bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .job_logs(job_id, after, limit)
            .await
    })
}

pub(crate) fn cancel_job(state_dir: &Path, job_id: u64) -> Result<(), bosn_service::Error> {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    runtime.run(async {
        ServiceClient::for_state(state_dir)?
            .cancel_job(job_id)
            .await
    })
}
