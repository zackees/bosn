//! The daemon process: builder seams and the serve loop.

use super::*;

impl Service {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            setup_executor: Arc::new(DockerSetupPrepareExecutor::new(state_dir.clone())),
            setup_task_executor: Arc::new(DockerSetupTaskExecutor::new(state_dir.clone())),
            setup_app_task_executor: Arc::new(DockerSetupAppTaskExecutor::new(state_dir.clone())),
            setup_ensure_executor: Arc::new(DockerSetupEnsureExecutor::new(state_dir.clone())),
            manifest_ensure_executor: Arc::new(DockerManifestEnsureExecutor::new(
                state_dir.clone(),
            )),
            manifest_app_task_executor: Arc::new(DockerManifestAppTaskExecutor::new(
                state_dir.clone(),
            )),
            setup_adopt_executor: Arc::new(DockerSetupAdoptExecutor::new(state_dir.clone())),
            doctor_executor: Arc::new(DockerDoctorExecutor::new()),
            setup_reconcile_executor: Arc::new(DockerSetupReconcileExecutor::new()),
            manifest_recovery_executor: Arc::new(DockerManifestRecoveryExecutor::new()),
            release_version: Arc::from(""),
            act_backend: Arc::new(ci::engine::DockerActBackend::default()),
            state_dir,
            stop: CancellationSource::new(),
        }
    }
    /// The release version this daemon reports on ping, so a client of another
    /// release can refuse it clearly instead of sending requests the daemon may
    /// misread. The `bosn` binary passes its own package version.
    pub fn with_release_version(mut self, version: impl Into<String>) -> Self {
        self.release_version = Arc::from(version.into());
        self
    }
    /// Substitute only the semantic setup executor. This is primarily an
    /// integration-test seam; production callers retain the Docker adapter.
    pub fn with_setup_prepare_executor(mut self, executor: Arc<dyn SetupPrepareExecutor>) -> Self {
        self.setup_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-task executor. This is an
    /// integration-test seam; it cannot add arbitrary Docker controls.
    pub fn with_setup_task_executor(mut self, executor: Arc<dyn SetupTaskExecutor>) -> Self {
        self.setup_task_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-app-task executor. This is
    /// a test seam; it cannot add Docker argv, a container target, or a task
    /// command to the caller-facing request.
    pub fn with_setup_app_task_executor(mut self, executor: Arc<dyn SetupAppTaskExecutor>) -> Self {
        self.setup_app_task_executor = executor;
        self
    }
    /// Substitute only the complete semantic setup-ensure executor. This is a
    /// test seam; it does not add a caller-controlled container operation.
    pub fn with_setup_ensure_executor(mut self, executor: Arc<dyn SetupEnsureExecutor>) -> Self {
        self.setup_ensure_executor = executor;
        self
    }
    /// Substitute the finite semantic manifest-runtime executor for tests.
    pub fn with_manifest_ensure_executor(
        mut self,
        executor: Arc<dyn ManifestEnsureExecutor>,
    ) -> Self {
        self.manifest_ensure_executor = executor;
        self
    }
    /// Substitute the complete semantic manifest app-task executor for tests.
    pub fn with_manifest_app_task_executor(
        mut self,
        executor: Arc<dyn ManifestAppTaskExecutor>,
    ) -> Self {
        self.manifest_app_task_executor = executor;
        self
    }
    pub fn with_setup_adopt_executor(mut self, executor: Arc<dyn SetupAdoptExecutor>) -> Self {
        self.setup_adopt_executor = executor;
        self
    }
    /// Substitute the fixed semantic doctor probe for deterministic tests.
    /// This is not an engine-command injection seam.
    pub fn with_doctor_executor(mut self, executor: Arc<dyn DoctorExecutor>) -> Self {
        self.doctor_executor = executor;
        self
    }
    pub fn with_setup_reconcile_executor(
        mut self,
        executor: Arc<dyn SetupReconcileExecutor>,
    ) -> Self {
        self.setup_reconcile_executor = executor;
        self
    }
    /// Substitute only the fixed inspect/start recovery seam for tests.
    pub fn with_manifest_recovery_executor(
        mut self,
        executor: Arc<dyn ManifestRecoveryExecutor>,
    ) -> Self {
        self.manifest_recovery_executor = executor;
        self
    }
    /// Substitute the isolated-engine runtime behind `bosn ci` (tests).
    pub fn with_act_backend(mut self, backend: Arc<dyn ci::engine::ActEngineBackend>) -> Self {
        self.act_backend = backend;
        self
    }
    /// Foreground lifecycle: acquires the sole registry writer before binding.
    pub async fn serve(self) -> Result<(), Error> {
        ipc::ensure_owner_private_directory(&self.state_dir)?;
        let db = self.state_dir.join("registry.sqlite3");
        let registry = match kernal_api::platform::fs::path_identity(&db) {
            Ok(Some(_)) => async_engine::launch_blocking(move || Registry::open_writer(&db))
                .await
                .map_err(|_| Error::ActorClosed)??,
            Ok(None) => return Err(Error::Protocol("registry identity unavailable")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let bytes = kernal_api::random::SecureRandom::new(1, IO_DEADLINE)
                    .map_err(|_| Error::Random)?
                    .bytes(16)
                    .await
                    .map_err(|_| Error::Random)?;
                async_engine::launch_blocking(move || Registry::create_writer(&db, &uuid(&bytes)))
                    .await
                    .map_err(|_| Error::ActorClosed)??
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let ep = endpoint(&self.state_dir)?;
        if ep.target_exists()? {
            retire_stale_socket(&ep)?;
        }
        let listener = AsyncListener::bind_owner_only(&ep)?;
        let (sender, receiver) = async_engine::channel(16);
        let actor = RegistryActor { sender };
        let (job_sender, job_receiver) = async_engine::channel(SETUP_PREPARE_COMMAND_QUEUE);
        let jobs = JobActor {
            sender: job_sender.clone(),
        };
        let job_worker = async_engine::launch(job_actor(
            Jobs::new(1),
            job_receiver,
            SetupExecutors {
                prepare: Arc::clone(&self.setup_executor),
                task: Arc::clone(&self.setup_task_executor),
                app_task: Arc::clone(&self.setup_app_task_executor),
                ensure: Arc::clone(&self.setup_ensure_executor),
                manifest_ensure: Arc::clone(&self.manifest_ensure_executor),
                manifest_app_task: Arc::clone(&self.manifest_app_task_executor),
            },
            job_sender.clone(),
            actor.clone(),
        ));
        let worker = async_engine::launch(registry_actor(
            registry,
            receiver,
            #[cfg(test)]
            None,
        ));
        // Recovery runs before accepting user requests so a newly submitted
        // task cannot race a stopped-container restart. Failure to inspect a
        // local Docker daemon is recorded as a bounded recovery outcome when
        // possible and never prevents the authenticated control plane from
        // coming up.
        let ci = ci::CiRuntime::start(
            &self.state_dir,
            actor.clone(),
            Arc::clone(&self.act_backend),
            ci::scheduler::default_limit(
                std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get),
            ),
        );
        // The opt-in dashboard listener lives as long as the daemon.
        let config = ci::config::load(&self.state_dir);
        if let Ok(config) = &config {
            ci.configure_widget(config.widget.clone());
        }
        let _ui = match config {
            Ok(config) => match ci::ui::start(config.ui, ci.clone()).await {
                Ok(Some(server)) => {
                    ci.attach_ui(Arc::clone(&server.handle));
                    ci.maybe_launch_widget(ci::widget::LaunchTrigger::DaemonStart);
                    eprintln!("bosn ci: dashboard listening on {}", server.handle.origin);
                    Some(server)
                }
                Ok(None) => None,
                Err(error) => {
                    eprintln!("bosn ci: dashboard listener failed to start: {error}");
                    None
                }
            },
            Err(error) => {
                eprintln!("bosn ci: dashboard disabled, config unreadable: {error}");
                None
            }
        };
        // Engines a previous daemon left behind: snapshot them before any
        // client can submit a run, then reconcile exactly that set in the
        // background, so a live run is never mistaken for a leftover.
        match ci.pending_engines().await {
            Ok(leftovers) if !leftovers.is_empty() => {
                let ci = ci.clone();
                async_engine::launch(async move {
                    let report = ci.reconcile_engines(&leftovers).await;
                    for (run, error) in report.failed {
                        eprintln!("bosn ci: engine for run {run} needs attention: {error}");
                    }
                })
                .detach();
            }
            Ok(_) => {}
            Err(error) => eprintln!("bosn ci: engine recovery skipped: {error}"),
        }
        let _ = recover_manifest_startup(
            &actor,
            &self.state_dir,
            self.manifest_recovery_executor.as_ref(),
        )
        .await;
        // Unattended maintenance. A machine that opted into autostart should learn about its
        // unowned Docker footprint on its own: #147 recorded 45 hours of normal use in which
        // nothing was ever said. The pass runs on the blocking pool because the census makes
        // bounded child-process calls, and the accept loop must not wait behind them. The
        // wait between passes is cancellable, so shutdown is not delayed by up to an hour.
        let _maintenance = {
            let state_dir = self.state_dir.clone();
            let stop = self.stop.token();
            async_engine::launch(async move {
                loop {
                    let state_dir = state_dir.clone();
                    let _ = async_engine::launch_blocking(move || {
                        let (scan, warning) = unmanaged::maintenance_pass(
                            &state_dir,
                            bosn_core::CensusConfig::default(),
                        );
                        match warning {
                            Some(warning) => {
                                for line in unmanaged::warning_lines(&warning) {
                                    eprintln!("bosn maintenance: {line}");
                                }
                            }
                            None if !scan.is_trustworthy() => eprintln!(
                                "bosn maintenance: the unmanaged census could not be read \
                                 completely, so this machine is not known to be clean"
                            ),
                            None => {}
                        }
                    })
                    .await;
                    if async_engine::cancellable(
                        &stop,
                        async_engine::sleep(unmanaged::MAINTENANCE_INTERVAL),
                    )
                    .await
                    .is_err()
                    {
                        break;
                    }
                }
            })
        };
        let mut clients = async_engine::TaskGroup::new();
        while !self.stop.is_cancelled() {
            // TaskGroup retains completed tasks until collected.  Reap only
            // ready completions: awaiting one here would let 32 slow peers
            // prevent the accept loop from serving everyone else.
            while matches!(
                async_engine::timeout(Duration::ZERO, clients.join_next()).await,
                Ok(Some(_))
            ) {}
            let accepted =
                async_engine::timeout(Duration::from_millis(100), listener.accept()).await;
            let stream = match accepted {
                Ok(Ok(stream)) => stream,
                Ok(Err(_)) => {
                    // A persistent listener failure must not turn this foreground
                    // process into a hot loop.  The listener remains owned, so
                    // yield before observing it again.
                    async_engine::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(_) => continue,
            };
            if clients.len() >= 32 {
                // Admission is bounded. Dropping this newly accepted stream is
                // intentional; do not wait for a slow peer to make capacity.
                continue;
            }
            let actor = actor.clone();
            let jobs = jobs.clone();
            let stop = self.stop.clone();
            let doctor = Arc::clone(&self.doctor_executor);
            let reconcile = Arc::clone(&self.setup_reconcile_executor);
            let adopt = Arc::clone(&self.setup_adopt_executor);
            let state_dir = self.state_dir.clone();
            let release_version = Arc::clone(&self.release_version);
            let ci = ci.clone();
            clients.spawn(async move {
                handle(
                    stream,
                    ConnectionContext {
                        actor,
                        jobs,
                        stop,
                        doctor,
                        adopt,
                        reconcile,
                        state_dir,
                        release_version,
                        ci,
                    },
                )
                .await
            });
        }
        while clients.join_next().await.is_some() {}
        // Keep the sole registry writer alive while the job actor cancels and
        // drains typed work: a shutdown-cancelled setup ensure still needs its
        // durable terminal audit event before the writer can be released.
        jobs.stop().await;
        drop(jobs);
        let _ = job_worker.await;
        actor.stop().await;
        let _ = worker.await;
        Ok(())
    }
}

pub struct Service {
    pub(crate) act_backend: Arc<dyn ci::engine::ActEngineBackend>,
    pub(crate) state_dir: PathBuf,
    pub(crate) stop: CancellationSource,
    pub(crate) setup_executor: Arc<dyn SetupPrepareExecutor>,
    pub(crate) setup_task_executor: Arc<dyn SetupTaskExecutor>,
    pub(crate) setup_app_task_executor: Arc<dyn SetupAppTaskExecutor>,
    pub(crate) setup_ensure_executor: Arc<dyn SetupEnsureExecutor>,
    pub(crate) manifest_ensure_executor: Arc<dyn ManifestEnsureExecutor>,
    pub(crate) manifest_app_task_executor: Arc<dyn ManifestAppTaskExecutor>,
    pub(crate) setup_adopt_executor: Arc<dyn SetupAdoptExecutor>,
    pub(crate) doctor_executor: Arc<dyn DoctorExecutor>,
    pub(crate) setup_reconcile_executor: Arc<dyn SetupReconcileExecutor>,
    pub(crate) manifest_recovery_executor: Arc<dyn ManifestRecoveryExecutor>,
    pub(crate) release_version: Arc<str>,
}
