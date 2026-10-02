//! The daemon operations MCP tools call, and the client-backed implementation.

use super::*;

pub(crate) trait Backend {
    /// The CI tools' daemon view (see [`crate::ci::mcp`]).
    fn ci(&mut self) -> Box<dyn crate::ci::mcp::CiBackend + '_>;
    fn status(&mut self) -> Result<Status, Error>;
    /// The runner accounting view (#358); see [`Client::jobs`].
    fn jobs(&mut self) -> Result<Value, Error> {
        Err(Error::Protocol("jobs view unavailable"))
    }
    fn doctor(&mut self) -> Result<DoctorReport, Error>;
    fn registry_resources(&mut self, after: u64, limit: u32)
    -> Result<RegistryResourcePage, Error>;
    fn setup_ensure_events(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error>;
    fn setup_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error>;
    fn manifest_volume_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error>;
    fn manifest_volume_release_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error>;
    fn setup_reconcile_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error>;
    fn setup_reconcile_repair_missing(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupReconcileMissingRepairResult, Error>;
    fn setup_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupGcApplyResult, Error>;
    fn manifest_volume_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error>;
    fn manifest_volume_release_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error>;
    fn setup_stop_retired(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupRetiredStopResult, Error>;
    fn setup_done(&mut self, workspace: PathBuf) -> Result<SetupDoneResult, Error>;
    fn setup_adopt(&mut self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error>;
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error>;
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error>;
    fn cancel_job(&mut self, id: u64) -> Result<(), Error>;
    /// Submit a bounded, semantic setup image-preparation job. The daemon,
    /// rather than the MCP process, owns all Docker interaction.
    fn submit_setup_prepare(&mut self, request: SetupPrepareRequest) -> Result<u64, Error>;
    /// Submit a bounded, semantic application ensure job. The daemon derives
    /// every lifecycle detail from the validated setup document and refuses a
    /// foreign or mismatched candidate rather than replacing it.
    fn submit_setup_ensure(&mut self, request: SetupEnsureJobRequest) -> Result<u64, Error>;
    fn submit_manifest_ensure(&mut self, request: ManifestEnsureJobRequest) -> Result<u64, Error>;
    fn submit_manifest_converge(
        &mut self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error>;
    fn submit_manifest_app_task(
        &mut self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error>;
    /// Submit one complete setup plan, image-preparation, and declared-task
    /// job.  The named task is the only executable selection exposed to MCP;
    /// the daemon derives all task details from the validated setup document.
    fn submit_setup_task(&mut self, request: SetupTaskJobRequest) -> Result<u64, Error>;
    fn submit_setup_app_task(&mut self, request: SetupAppTaskJobRequest) -> Result<u64, Error>;
    /// Generate an inert setup receipt under the state root selected when the
    /// MCP process was started.  Tool arguments intentionally cannot replace
    /// that root.
    fn setup_plan(
        &mut self,
        workspace: PathBuf,
        locator: String,
        policy: SetupAcquirePolicy,
    ) -> Result<SetupPlan, Error>;
}

pub(crate) struct DaemonBackend<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) client: Client,
    pub(crate) state_dir: PathBuf,
}
impl Backend for DaemonBackend<'_> {
    fn ci(&mut self) -> Box<dyn crate::ci::mcp::CiBackend + '_> {
        Box::new(crate::ci::mcp::ClientCi::new(self.runtime, &self.client))
    }
    fn status(&mut self) -> Result<Status, Error> {
        self.runtime.run(self.client.status())
    }
    fn jobs(&mut self) -> Result<Value, Error> {
        self.runtime.run(self.client.jobs())
    }
    fn doctor(&mut self) -> Result<DoctorReport, Error> {
        self.runtime.run(self.client.doctor())
    }
    fn registry_resources(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        self.runtime
            .run(self.client.registry_resources(after, limit))
    }
    fn setup_ensure_events(
        &mut self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        self.runtime
            .run(self.client.setup_ensure_events(after, limit))
    }
    fn setup_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        self.runtime
            .run(self.client.setup_gc_preview(workspace, after, limit))
    }
    fn manifest_volume_gc_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_gc_preview(workspace, after, limit),
        )
    }
    fn manifest_volume_release_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_release_preview(workspace, after, limit),
        )
    }
    fn setup_reconcile_preview(
        &mut self,
        workspace: PathBuf,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error> {
        self.runtime
            .run(self.client.setup_reconcile_preview(workspace, after, limit))
    }
    fn setup_reconcile_repair_missing(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupReconcileMissingRepairResult, Error> {
        self.runtime.run(
            self.client
                .setup_reconcile_repair_missing(workspace, &token, true),
        )
    }
    fn setup_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupGcApplyResult, Error> {
        self.runtime
            .run(self.client.setup_gc_apply(workspace, &token, true))
    }
    fn manifest_volume_gc_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_gc_apply(workspace, &token, true),
        )
    }
    fn manifest_volume_release_apply(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        self.runtime.run(
            self.client
                .manifest_volume_release_apply(workspace, &token, true),
        )
    }
    fn setup_stop_retired(
        &mut self,
        workspace: PathBuf,
        token: String,
    ) -> Result<SetupRetiredStopResult, Error> {
        self.runtime
            .run(self.client.setup_stop_retired(workspace, &token, true))
    }
    fn setup_done(&mut self, workspace: PathBuf) -> Result<SetupDoneResult, Error> {
        self.runtime.run(self.client.setup_done(workspace, true))
    }
    fn setup_adopt(&mut self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error> {
        self.runtime.run(self.client.setup_adopt(request))
    }
    fn job_status(&mut self, id: u64) -> Result<JobStatus, Error> {
        self.runtime.run(self.client.job_status(id))
    }
    fn job_logs(&mut self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
        self.runtime.run(self.client.job_logs(id, after, limit))
    }
    fn cancel_job(&mut self, id: u64) -> Result<(), Error> {
        self.runtime.run(self.client.cancel_job(id))
    }
    fn submit_setup_prepare(&mut self, request: SetupPrepareRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_prepare(request))
    }
    fn submit_setup_ensure(&mut self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_ensure(request))
    }
    fn submit_manifest_ensure(&mut self, request: ManifestEnsureJobRequest) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_ensure(request))
    }
    fn submit_manifest_converge(
        &mut self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_converge(request))
    }
    fn submit_manifest_app_task(
        &mut self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        self.runtime
            .run(self.client.submit_manifest_app_task(request))
    }
    fn submit_setup_task(&mut self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_task(request))
    }
    fn submit_setup_app_task(&mut self, request: SetupAppTaskJobRequest) -> Result<u64, Error> {
        self.runtime.run(self.client.submit_setup_app_task(request))
    }
    fn setup_plan(
        &mut self,
        workspace: PathBuf,
        locator: String,
        policy: SetupAcquirePolicy,
    ) -> Result<SetupPlan, Error> {
        // Keep detailed filesystem/remote errors out of the MCP boundary.
        self.runtime
            .run(plan_setup(SetupPlanRequest {
                state_dir: self.state_dir.clone(),
                workspace,
                locator,
                policy,
            }))
            .map_err(|_| Error::Protocol("setup plan failed"))
    }
}
