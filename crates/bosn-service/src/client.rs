//! The daemon client: one typed request per call over the owner-only endpoint.

use super::*;

#[derive(Clone, Debug)]
pub struct Client {
    pub(crate) state_dir: PathBuf,
}
impl Client {
    pub fn for_state(state: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            state_dir: state.as_ref().to_path_buf(),
        })
    }
    pub async fn ping(&self) -> Result<(), Error> {
        self.daemon_version().await.map(drop)
    }
    /// Ping the daemon and return the release version it reports. Empty means
    /// a daemon that predates the version handshake (bosn 0.1.5 and older).
    /// Compare it with [`daemon_version_mismatch`] before submitting work.
    pub async fn daemon_version(&self) -> Result<String, Error> {
        match self.call(Request::operation(1)).await? {
            Reply::Pong(version) => Ok(version),
            _ => Err(Error::Protocol("unexpected ping response")),
        }
    }
    pub async fn status(&self) -> Result<Status, Error> {
        match self.call(Request::operation(2)).await? {
            Reply::Status(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected status response")),
        }
    }
    /// Perform the fixed daemon-owned read-only registry integrity and Docker
    /// version checks. An absent/unreachable daemon is a stable outcome, not a
    /// client-side registry open or an unredacted transport error.
    pub async fn doctor(&self) -> Result<DoctorReport, Error> {
        match self.call(Request::operation(13)).await {
            Ok(Reply::Doctor(v)) => Ok(v),
            Ok(_) => Err(Error::Protocol("unexpected doctor response")),
            Err(
                Error::Deadline | Error::Io(_) | Error::ActorClosed | Error::EndpointOccupied(_),
            ) => Ok(DoctorReport::daemon_unavailable()),
            Err(error) => Err(error),
        }
    }
    /// Read a bounded, path-safe page of managed resource diagnostics from the
    /// already-running daemon. This never opens, creates, or migrates a
    /// registry in the client process.
    pub async fn registry_resources(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<RegistryResourcePage, Error> {
        validate_registry_page(after, limit)?;
        match self
            .call(Request {
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(11)
            })
            .await?
        {
            Reply::RegistryResources(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected registry resources response")),
        }
    }
    /// Read a bounded, newest-first page of credential-safe setup ensure and
    /// native-manifest lifecycle history from the already-running daemon.
    /// This has no registry write path and never initializes state in the
    /// client process.
    pub async fn setup_ensure_events(
        &self,
        after: u64,
        limit: u32,
    ) -> Result<SetupEnsureEventPage, Error> {
        validate_registry_page(after, limit)?;
        match self
            .call(Request {
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(12)
            })
            .await?
        {
            Reply::SetupEnsureEvents(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup ensure events response")),
        }
    }
    /// Return a read-only, conservative future-GC preview for one exact
    /// workspace. No client-side registry open, Docker call, or mutation is
    /// possible through this method.
    pub async fn setup_gc_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<SetupGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty()
            || workspace.len() > 8 * 1024
            || workspace.bytes().any(|byte| byte == 0)
        {
            return Err(Error::Protocol("invalid setup gc preview workspace"));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(14)
            })
            .await?
        {
            Reply::SetupGcPreview(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup gc preview response")),
        }
    }
    /// Re-derive the unmanaged plan in the daemon and remove it in one bounded pass.
    ///
    /// The caller's preview is never trusted. The daemon takes a fresh census and rebuilds
    /// the plan immediately before removing anything, and refuses outright if that census is
    /// incomplete, because an unreadable census is never a reason to delete.
    pub async fn unmanaged_gc_apply(
        &self,
        include: Vec<String>,
        ttl_seconds: u64,
        confirm: bool,
    ) -> Result<UnmanagedApplySummary, Error> {
        if !confirm {
            return Err(Error::Protocol("unmanaged apply requires confirmation"));
        }
        match self
            .call(Request {
                gc_confirm: true,
                unmanaged_include: include,
                unmanaged_ttl_seconds: ttl_seconds,
                ..Request::operation(35)
            })
            .await?
        {
            Reply::UnmanagedApply(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected unmanaged apply response")),
        }
    }

    /// Preview only retired disposable native-manifest volumes. Stack,
    /// machine, and pinned data are excluded by policy.
    pub async fn manifest_volume_gc_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty() || workspace.len() > 8 * 1024 || workspace.bytes().any(|b| b == 0) {
            return Err(Error::Protocol(
                "invalid manifest volume gc preview workspace",
            ));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(31)
            })
            .await?
        {
            Reply::ManifestVolumeGcPreview(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected manifest volume gc preview response",
            )),
        }
    }
    /// Preview durable manifest volumes which can only be removed through the
    /// explicit release contract. Normal volume GC never returns these rows.
    pub async fn manifest_volume_release_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<ManifestVolumeGcPreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty() || workspace.len() > 8 * 1024 || workspace.bytes().any(|b| b == 0) {
            return Err(Error::Protocol(
                "invalid manifest volume release preview workspace",
            ));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(33)
            })
            .await?
        {
            Reply::ManifestVolumeGcPreview(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected manifest volume release preview response",
            )),
        }
    }
    /// Compare durable Bosn-managed setup container facts with fixed Docker
    /// inspection. This is read-only: it never creates, opens, writes, or
    /// migrates a registry and has no repair/apply operation.
    pub async fn setup_reconcile_preview(
        &self,
        workspace: impl AsRef<Path>,
        after: u64,
        limit: u32,
    ) -> Result<SetupReconcilePreviewPage, Error> {
        validate_registry_page(after, limit)?;
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        if workspace.is_empty()
            || workspace.len() > 8 * 1024
            || workspace.bytes().any(|byte| byte == 0)
        {
            return Err(Error::Protocol("invalid setup reconcile preview workspace"));
        }
        match self
            .call(Request {
                workspace,
                diagnostic_after: after,
                diagnostic_limit: limit,
                ..Request::operation(19)
            })
            .await?
        {
            Reply::SetupReconcilePreview(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected setup reconcile preview response",
            )),
        }
    }
    /// Retire exactly one previewed active setup app only after fixed Docker
    /// inspection still proves it absent. This never accepts an engine name,
    /// image, argv, mount, or lifecycle control.
    pub async fn setup_reconcile_repair_missing(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupReconcileMissingRepairResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_reconcile_repair_missing_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(20)
            })
            .await?
        {
            Reply::SetupReconcileMissingRepair(value) => Ok(value),
            _ => Err(Error::Protocol(
                "unexpected setup reconcile repair response",
            )),
        }
    }
    /// Apply exactly one opaque candidate returned by a preceding GC preview.
    /// Docker and registry mutation remain daemon-owned; no raw engine target
    /// crosses this boundary.
    pub async fn setup_gc_apply(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupGcApplyResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_gc_apply_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(15)
            })
            .await?
        {
            Reply::SetupGcApply(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup gc apply response")),
        }
    }
    /// Remove exactly one preview-token-bound disposable manifest volume after
    /// registry, labels, and attachment revalidation by the daemon.
    pub async fn manifest_volume_gc_apply(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_manifest_volume_gc_apply_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(32)
            })
            .await?
        {
            Reply::ManifestVolumeGcApply(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected manifest volume gc apply response",
            )),
        }
    }
    /// Destructively release exactly one preview-token-bound durable manifest
    /// volume. The daemon owns every Docker argument and rechecks registry,
    /// label, and attachment facts immediately before removal.
    pub async fn manifest_volume_release_apply(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<ManifestVolumeGcApplyResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_manifest_volume_release_apply_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(34)
            })
            .await?
        {
            Reply::ManifestVolumeGcApply(v) => Ok(v),
            _ => Err(Error::Protocol(
                "unexpected manifest volume release apply response",
            )),
        }
    }
    /// Stop exactly one opaque retired candidate returned by GC preview. The
    /// daemon revalidates durable ownership and fixed Docker labels; the
    /// caller cannot select a Docker name, image, argv, or timeout.
    pub async fn setup_stop_retired(
        &self,
        workspace: impl AsRef<Path>,
        candidate_token: &str,
        confirm: bool,
    ) -> Result<SetupRetiredStopResult, Error> {
        let workspace = workspace.as_ref().to_string_lossy().into_owned();
        validate_setup_retired_stop_input(&workspace, candidate_token, confirm)?;
        match self
            .call(Request {
                workspace,
                gc_candidate_token: candidate_token.into(),
                gc_confirm: true,
                ..Request::operation(18)
            })
            .await?
        {
            Reply::SetupRetiredStop(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup retired stop response")),
        }
    }
    /// Explicitly mark this setup workspace's active registry ownership done.
    /// This does not contact Docker or remove any resource. `confirm` is
    /// required so normal inspection cannot accidentally change lifecycle
    /// accounting.
    pub async fn setup_done(
        &self,
        workspace: impl AsRef<Path>,
        confirm: bool,
    ) -> Result<SetupDoneResult, Error> {
        if !confirm {
            return Err(Error::Protocol("invalid setup done request"));
        }
        let workspace = canonical_setup_done_workspace(workspace)?;
        match self
            .call(Request {
                workspace,
                setup_done_confirm: true,
                ..Request::operation(16)
            })
            .await?
        {
            Reply::SetupDone(value) => Ok(value),
            _ => Err(Error::Protocol("unexpected setup done response")),
        }
    }
    /// Confirmed, daemon-owned restoration of registry facts for one existing
    /// managed setup app. This never accepts a container/image selector.
    pub async fn setup_adopt(&self, request: SetupAdoptRequest) -> Result<SetupAdoptResult, Error> {
        let workspace = request.workspace.to_string_lossy().into_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_adopt_input(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
            request.confirm,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                setup_adopt_confirm: true,
                ..Request::operation(17)
            })
            .await?
        {
            Reply::SetupAdopt(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected setup adopt response")),
        }
    }
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.call(Request::operation(3)).await? {
            Reply::Shutdown => Ok(()),
            _ => Err(Error::Protocol("unexpected shutdown response")),
        }
    }
    /// Shut the daemon down and wait, up to `limit`, until it no longer
    /// answers, so the next command starts a fresh daemon instead of reaching
    /// one that is winding down.
    pub async fn shutdown_and_wait(&self, limit: Duration) -> Result<(), Error> {
        self.shutdown().await?;
        let deadline = async_engine::Deadline::after(limit);
        while self.ping().await.is_ok() {
            if deadline.remaining().is_zero() {
                return Err(Error::Deadline);
            }
            async_engine::sleep(Duration::from_millis(20)).await;
        }
        Ok(())
    }
    pub async fn submit_job(
        &self,
        workspace: &str,
        stack: &str,
        digest: &str,
    ) -> Result<u64, Error> {
        match self
            .call(Request {
                workspace: workspace.into(),
                stack: stack.into(),
                digest: digest.into(),
                ..Request::operation(4)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected submit response")),
        }
    }
    pub async fn job_status(&self, id: u64) -> Result<JobStatus, Error> {
        match self
            .call(Request {
                job_id: id,
                ..Request::operation(5)
            })
            .await?
        {
            Reply::JobStatus(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected job status response")),
        }
    }
    /// The daemon's runner accounting (#358): capacity, lane load, and every
    /// queued, running and recently finished job with what it holds, as the
    /// JSON document `bosn jobs --json` prints.
    pub async fn jobs(&self) -> Result<serde_json::Value, Error> {
        match self.call(Request::operation(37)).await? {
            Reply::Jobs(json) => {
                serde_json::from_str(&json).map_err(|_| Error::Protocol("malformed jobs view"))
            }
            _ => Err(Error::Protocol("unexpected jobs response")),
        }
    }
    pub async fn cancel_job(&self, id: u64) -> Result<(), Error> {
        match self
            .call(Request {
                job_id: id,
                ..Request::operation(6)
            })
            .await?
        {
            Reply::Cancelled => Ok(()),
            _ => Err(Error::Protocol("unexpected job cancel response")),
        }
    }
    pub async fn job_logs(&self, id: u64, after: u64, limit: u32) -> Result<JobLogPage, Error> {
        match self
            .call(Request {
                job_id: id,
                log_after: after,
                log_limit: limit,
                ..Request::operation(7)
            })
            .await?
        {
            Reply::JobLogs(v) => Ok(v),
            _ => Err(Error::Protocol("unexpected job logs response")),
        }
    }
    /// Submit a daemon-owned plan-and-image-prepare operation. The response is
    /// only the durable job ID; Docker work happens after the authenticated
    /// reply and is observed through status/log polling.
    pub async fn submit_setup_prepare(&self, request: SetupPrepareRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_prepare_wire(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(8)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup prepare response")),
        }
    }
    /// Submit one daemon-owned plan, image-prepare, and declared-task job.
    /// The response is the durable job ID; callers observe the bounded work
    /// through the existing status/log/cancel methods.
    pub async fn submit_setup_task(&self, request: SetupTaskJobRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_task_wire(
            &workspace,
            &request.config,
            request.policy,
            &request.task_name,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_task_name: request.task_name,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(9)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup task response")),
        }
    }
    /// Submit one task declared by a setup document for execution inside its
    /// already ensured, ownership-verified application container.
    pub async fn submit_setup_app_task(
        &self,
        request: SetupAppTaskJobRequest,
    ) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_task_wire(
            &workspace,
            &request.config,
            request.policy,
            &request.task_name,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_task_name: request.task_name,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(21)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup app task response")),
        }
    }
    /// Submit one daemon-owned plan, image-prepare, and ownership-safe
    /// application ensure job. The returned ID can only be observed with the
    /// existing bounded status/log/cancel APIs.
    pub async fn submit_setup_ensure(&self, request: SetupEnsureJobRequest) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("setup workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("setup deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("setup output limit too large"))?;
        validate_setup_ensure_wire(
            &workspace,
            &request.config,
            request.policy,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                setup_config: request.config,
                setup_policy: request.policy.wire(),
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(10)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected setup ensure response")),
        }
    }
    /// Submit one explicit manifest stack runtime ensure. The selected stack
    /// is a manifest declaration, never a Docker target or command.
    pub async fn submit_manifest_ensure(
        &self,
        request: ManifestEnsureJobRequest,
    ) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("manifest workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("manifest deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("manifest output limit too large"))?;
        validate_manifest_ensure_wire(
            &workspace,
            &request.manifest,
            &request.stack,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                stack: request.stack,
                setup_config: request.manifest,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(22)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected manifest ensure response")),
        }
    }
    /// Submit one deterministic convergence of every stack in a manifest.
    /// The daemon re-reads the manifest before any engine work; callers cannot
    /// select an ordering, dependency, Docker target, or lifecycle option.
    pub async fn submit_manifest_converge(
        &self,
        request: ManifestConvergeJobRequest,
    ) -> Result<u64, Error> {
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("manifest workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("manifest deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("manifest output limit too large"))?;
        validate_manifest_converge_wire(&workspace, &request.manifest, deadline_ms, output_limit)?;
        match self
            .call(Request {
                workspace,
                setup_config: request.manifest,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                ..Request::operation(24)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected manifest converge response")),
        }
    }
    /// Submit one named task for an already ensured supported manifest stack.
    /// The task name is the only executable selector; manifest command and
    /// managed container identity are re-derived by the daemon.
    pub async fn submit_manifest_app_task(
        &self,
        request: ManifestAppTaskJobRequest,
    ) -> Result<u64, Error> {
        self.submit_manifest_app_task_with_lease(request, None)
            .await
    }
    /// Submit like [`Self::submit_manifest_app_task`], for a caller that
    /// follows the job to its end by polling `job_status`/`job_logs`. The
    /// daemon cancels the job (queued or running) once no poll has arrived
    /// for `lease`, so a follower killed by SIGTERM, SIGHUP or SIGKILL does
    /// not leave its job running to the deadline (#357). The lease must be
    /// within [`FOLLOW_LEASE_MIN`]..=[`FOLLOW_LEASE_MAX`].
    pub async fn follow_manifest_app_task(
        &self,
        request: ManifestAppTaskJobRequest,
        lease: Duration,
    ) -> Result<u64, Error> {
        if !(FOLLOW_LEASE_MIN..=FOLLOW_LEASE_MAX).contains(&lease) {
            return Err(Error::Protocol("invalid manifest app task follow lease"));
        }
        self.submit_manifest_app_task_with_lease(request, Some(lease))
            .await
    }
    async fn submit_manifest_app_task_with_lease(
        &self,
        request: ManifestAppTaskJobRequest,
        lease: Option<Duration>,
    ) -> Result<u64, Error> {
        let follow_lease_ms = lease.map_or(0, |lease| lease.as_millis() as u64);
        let workspace = request
            .workspace
            .to_str()
            .ok_or(Error::Protocol("manifest workspace is not UTF-8"))?
            .to_owned();
        let deadline_ms = u64::try_from(request.deadline.as_millis())
            .map_err(|_| Error::Protocol("manifest deadline too large"))?;
        let output_limit = u32::try_from(request.output_limit)
            .map_err(|_| Error::Protocol("manifest output limit too large"))?;
        validate_manifest_app_task_wire(
            &workspace,
            &request.manifest,
            &request.stack,
            &request.task_name,
            deadline_ms,
            output_limit,
        )?;
        match self
            .call(Request {
                workspace,
                stack: request.stack,
                setup_config: request.manifest,
                setup_task_name: request.task_name,
                setup_deadline_ms: deadline_ms,
                setup_output_limit: output_limit,
                follow_lease_ms,
                ..Request::operation(23)
            })
            .await?
        {
            Reply::Job(id) => Ok(id),
            _ => Err(Error::Protocol("unexpected manifest app task response")),
        }
    }
    /// One CI request; the reply is parsed eagerly into `T`, the request's
    /// typed reply (see [`ci::reply`]).
    pub(crate) async fn ci_call<T: serde::de::DeserializeOwned>(
        &self,
        request: ci::CiRequest,
    ) -> Result<T, Error> {
        let encoded =
            serde_json::to_string(&request).map_err(|_| Error::Protocol("ci request encode"))?;
        let deadline = request.reply_deadline();
        match self
            .call_within(
                Request {
                    ci_request: encoded,
                    ..Request::operation(36)
                },
                deadline,
            )
            .await?
        {
            Reply::Ci(json) => {
                serde_json::from_str(&json).map_err(|_| Error::Protocol("ci reply decode"))
            }
            Reply::CiError(json) => {
                let error: ci::ErrorReply =
                    serde_json::from_str(&json).map_err(|_| Error::Protocol("ci error decode"))?;
                Err(Error::Ci {
                    code: error.code,
                    message: error.message,
                })
            }
            _ => Err(Error::Protocol("unexpected ci response")),
        }
    }
    /// Snapshot `workspace` into the daemon's staging area, then submit it.
    /// The snapshot is taken here (same user) because copying a tree can
    /// take longer than one daemon request may.
    pub async fn ci_submit(&self, options: ci::SubmitOptions) -> Result<ci::SubmitReply, Error> {
        let request = ci::stage_submission(&self.state_dir, options).await?;
        let staging = ci::staging_root(&self.state_dir).join(&request.staging);
        let result = self
            .ci_call(ci::CiRequest::Submit {
                request: Box::new(request),
            })
            .await;
        // Only a refusal proves the daemon will not use the snapshot; after a
        // lost reply it may have queued the run, and its startup sweep drops
        // abandoned staging anyway.
        if daemon_refused(&result) {
            let _ = std::fs::remove_dir_all(staging);
        }
        result
    }
    pub async fn ci_list(
        &self,
        workspace: Option<String>,
        state: Option<ci::RunState>,
        limit: Option<usize>,
    ) -> Result<ci::ListReply, Error> {
        self.ci_call(ci::CiRequest::List {
            workspace,
            state,
            limit,
        })
        .await
    }
    pub async fn ci_show(&self, run: String, tree: bool) -> Result<ci::RunView, Error> {
        self.ci_call(ci::CiRequest::Show {
            run,
            tree: Some(tree),
        })
        .await
    }
    pub async fn ci_logs(&self, query: ci::LogsQuery) -> Result<ci::LogsReply, Error> {
        self.ci_call(ci::CiRequest::Logs {
            run: query.run,
            job: query.job,
            section: query.section,
            since_seq: Some(query.since_seq),
            limit: query.limit,
            max_bytes: query.max_bytes,
        })
        .await
    }
    pub async fn ci_cancel(&self, run: String) -> Result<ci::CancelReply, Error> {
        self.ci_call(ci::CiRequest::Cancel { run }).await
    }
    pub async fn ci_retry(
        &self,
        run: String,
        job: Option<String>,
    ) -> Result<ci::SubmitReply, Error> {
        self.ci_call(ci::CiRequest::Retry { run, job }).await
    }
    pub async fn ci_report(
        &self,
        run: String,
        tail: Option<usize>,
    ) -> Result<ci::RunReport, Error> {
        self.ci_call(ci::CiRequest::Report { run, tail }).await
    }
    /// A widget request (`WidgetHello`, `WidgetPoll`, `WidgetDismiss`,
    /// `WidgetCommand`); every one answers with the widget's state.
    pub async fn ci_widget(&self, request: ci::CiRequest) -> Result<ci::WidgetReply, Error> {
        self.ci_call(request).await
    }
    pub async fn ci_ui_grant(&self, path: Option<String>) -> Result<ci::UiGrantReply, Error> {
        self.ci_call(ci::CiRequest::UiGrant { path }).await
    }
    pub async fn ci_runners(&self, action: ci::RunnerAction) -> Result<ci::RunnersReply, Error> {
        self.ci_call(ci::CiRequest::Runners { action }).await
    }
    pub(crate) async fn call(&self, request: Request) -> Result<Reply, Error> {
        self.call_within(request, IO_DEADLINE).await
    }
    /// One request whose reply may take up to `reply_deadline` to arrive.
    async fn call_within(
        &self,
        request: Request,
        reply_deadline: Duration,
    ) -> Result<Reply, Error> {
        // Resolve on every call: a Client may have been constructed while a
        // fresh daemon was still creating its registry, before an inode-based
        // alias-stable endpoint name existed.
        let ep = endpoint(&self.state_dir)?;
        let mut stream = async_engine::timeout(IO_DEADLINE, AsyncStream::connect(&ep))
            .await
            .map_err(|_| Error::Deadline)??;
        if !peer_is_authorized(&stream.peer_identity()?.user_id, &ipc::current_user_id()?) {
            return Err(Error::Unauthorized);
        }
        let mut payload = Vec::new();
        request
            .encode(&mut payload)
            .map_err(|_| Error::Protocol("encode"))?;
        write_frame(
            &mut stream,
            DaemonFrame::request(PAYLOAD_PROTOCOL, payload).with_request_id(1),
        )
        .await?;
        let frame = read_frame_within(&mut stream, reply_deadline).await?;
        decode_response_frame(frame, 1)
    }
}

/// Whether the daemon definitely refused a CI request.
fn daemon_refused<T>(result: &Result<T, Error>) -> bool {
    matches!(result, Err(Error::Ci { .. }))
}

#[cfg(test)]
mod ci_tests {
    use super::*;

    #[test]
    fn only_a_definite_refusal_discards_the_staged_snapshot() {
        let refused: Result<(), Error> = Err(Error::Ci {
            code: "refused".into(),
            message: "no".into(),
        });
        assert!(daemon_refused(&refused));
        // The daemon may have accepted the run before the reply was lost.
        assert!(!daemon_refused::<()>(&Err(Error::Deadline)));
        assert!(!daemon_refused::<()>(&Err(Error::Protocol(
            "ci reply decode"
        ))));
        assert!(!daemon_refused(&Ok(())));
    }
}
