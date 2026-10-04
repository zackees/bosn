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
            capacity: None,
        }
    }
    /// Runner capacity (#358) for this daemon, overriding `runners.toml`
    /// and the `BOSN_RUNNER_*` environment.
    pub fn with_runner_capacity(mut self, capacity: capacity::RunnerCapacity) -> Self {
        self.capacity = Some(capacity);
        self
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
    #[expect(
        clippy::cognitive_complexity,
        clippy::too_many_lines,
        reason = "baseline, ci.yml#229"
    )]
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
        let act_owner = registry.registry_id()?;
        let act_image_proofs = act_engine::bundled_engine_manifests()
            .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?;
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
        let capacity = match self.capacity.clone() {
            Some(capacity) => capacity,
            None => {
                #[allow(unused_mut)]
                let mut loaded = capacity::RunnerCapacity::load(&self.state_dir)
                    .map_err(|error| Error::Io(std::io::Error::other(error)))?;
                // Unit-test daemons never route through a real proxy unless
                // a test asks for it with `with_runner_capacity`.
                #[cfg(test)]
                {
                    loaded.docker_proxy = false;
                }
                loaded
            }
        };
        DOCKER_PROXY_ENABLED.store(capacity.docker_proxy, std::sync::atomic::Ordering::Relaxed);
        eprintln!("bosn runners: {}", capacity.describe());
        let runners = Arc::new(runners::Runners::new(&self.state_dir, capacity.clone()));
        if let Some(dir) = runners.proxy_dir() {
            runners::ensure_proxy_dir(dir)?;
        }
        // Reap what a previous daemon process left running before any new
        // job can start (see `runners`): their streams and leases died with
        // it, so they can only be torn down, never re-adopted.
        {
            let runners = Arc::clone(&runners);
            let _ = async_engine::launch_blocking(move || reap_orphaned_runs(&runners)).await;
        }
        let job_worker = async_engine::launch(job_actor(
            Jobs::with_policy(jobs::SchedulerPolicy {
                runner_slots: capacity.runner_slots,
                control_slots: capacity.control_slots,
            }),
            job_receiver,
            SetupExecutors {
                prepare: Arc::clone(&self.setup_executor),
                task: Arc::clone(&self.setup_task_executor),
                app_task: Arc::clone(&self.setup_app_task_executor),
                ensure: Arc::clone(&self.setup_ensure_executor),
                manifest_ensure: Arc::clone(&self.manifest_ensure_executor),
                manifest_app_task: Arc::clone(&self.manifest_app_task_executor),
                runners: Some(runners),
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
        // The sole registry writer fences stale Act claims before this daemon
        // accepts any new execution. A failed seal is fatal: the control plane
        // must never admit work while startup interruption remains available.
        let act_recovery = act_runtime::recover_startup_act_engines(
            &actor,
            &DockerEngine::docker(),
            &act_owner,
            &act_image_proofs,
            act_runtime::ActStartupRecoveryOptions {
                page_size: 64,
                max_runs: 10000,
                deadline: Duration::from_secs(120),
            },
            &self.stop.token(),
        )
        .await;
        match act_recovery {
            Ok(report) => {
                for run in report.runs {
                    if let Some(reason) = run.deferred_reason {
                        eprintln!(
                            "bosn Act startup cleanup deferred for {}: {reason}",
                            run.run_id
                        );
                    }
                }
            }
            Err(error) => {
                jobs.stop().await;
                drop(jobs);
                let _ = job_worker.await;
                actor.stop().await;
                let _ = worker.await;
                return Err(Error::Io(error));
            }
        }
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
        // Retry durable cleanup online; failed retirement must not wait for restart.
        // This worker never uses startup interruption authority or shared prune.
        let _ci_cleanup = {
            let ci = ci.clone();
            let owner = act_owner.clone();
            let stop = self.stop.token();
            async_engine::launch(async move {
                let mut cursor = None;
                loop {
                    if stop.is_cancelled() {
                        break;
                    }
                    let retry = async_engine::timeout(
                        Duration::from_secs(200),
                        ci.retry_cleanup(&owner, cursor.clone()),
                    );
                    let pass = async_engine::cancellable(&stop, retry).await;
                    match pass {
                        Ok(Ok(Ok(report))) => {
                            cursor = report.next_cursor;
                            if let Some(run) = report.retired {
                                eprintln!("bosn CI cleanup retry removed {run}");
                            }
                            if let Some(reason) = report.deferred {
                                eprintln!(
                                    "bosn CI cleanup retry deferred for {}: {reason}",
                                    cursor.as_deref().unwrap_or("unknown")
                                );
                            }
                        }
                        Ok(Ok(Err(error))) => eprintln!("bosn CI cleanup retry failed: {error}"),
                        Ok(Err(_)) => eprintln!("bosn CI cleanup retry pass deadline exceeded"),
                        Err(_) => break,
                    }
                    if async_engine::cancellable(
                        &stop,
                        async_engine::sleep(Duration::from_secs(60)),
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
        // However the daemon was stopped, its spare engine (#410) goes too;
        // one left behind is retired by the next daemon's startup recovery.
        let _ =
            async_engine::timeout(crate::dispatch::SPARE_CLOSE_DEADLINE, ci.close_spares()).await;
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
    /// Runner capacity from `bosn daemon serve` flags; `None` loads
    /// `runners.toml` and the environment at serve time.
    pub(crate) capacity: Option<capacity::RunnerCapacity>,
}

/// Stop every process in a setup container except its idle PID 1, the idle
/// loop's own `sleep 3600` (see [`MANIFEST_LINUX_IDLE_COMMAND`]) and this
/// script: TERM, two seconds' grace, then KILL. Plain POSIX sh, because
/// dash's `kill` rejects the `-1` (all processes) target. Prints
/// `signalled=<n> left=<n>`.
const REAP_SCRIPT: &str = r#"self=$$
idle() { [ "$(tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null)" = "sleep 3600 " ]; }
targets() { for p in /proc/[0-9]*; do p=${p#/proc/}; [ "$p" = 1 ] || [ "$p" = "$self" ] || idle "$p" || echo "$p"; done; }
n=0; for p in $(targets); do kill -s TERM "$p" 2>/dev/null && n=$((n+1)); done
[ "$n" = 0 ] || sleep 2
for p in $(targets); do kill -s KILL "$p" 2>/dev/null; done
sleep 0.2; left=0; for p in $(targets); do [ -d "/proc/$p" ] && left=$((left+1)); done
echo "signalled=$n left=$left""#;

/// Tear down every run a previous daemon process recorded, then any run
/// carrying this daemon's label that no ledger names. Returns one line per
/// reaped run, also written to stderr.
pub(crate) fn reap_orphaned_runs(runners: &runners::Runners) -> Vec<String> {
    let mut lines = Vec::new();
    let mut reaped = BTreeSet::new();
    for record in runners.orphaned_runs() {
        let mut detail = String::new();
        if let Some(container) = &record.container {
            // Only one task job runs in a setup container at a time, and no
            // job runs yet: every process but the container's idle PID 1 is
            // the dead daemon's task.
            let stopped = DockerEngine::docker()
                .with_args([
                    "container",
                    "exec",
                    container.as_str(),
                    "sh",
                    "-c",
                    REAP_SCRIPT,
                ])
                .capture(RunOptions::bounded(Duration::from_secs(20), 4096));
            match stopped {
                Ok(result) if result.ok() => {
                    let text = String::from_utf8_lossy(&result.stdout);
                    let count = |key: &str| {
                        text.split_whitespace()
                            .find_map(|word| word.strip_prefix(key))
                            .unwrap_or("?")
                            .to_owned()
                    };
                    detail.push_str(&format!(
                        "signalled {} task process(es), {} still running; ",
                        count("signalled="),
                        count("left=")
                    ));
                }
                _ => detail.push_str("its container was not running; "),
            }
        }
        let teardown = runners.teardown(&record.run);
        detail.push_str(&teardown.summary());
        let line = format!(
            "reaped orphaned job {} ({} {} in {}): {detail}",
            record.job_id, record.stack, record.task, record.workspace
        );
        eprintln!("bosn runners: {line}");
        lines.push(line);
        reaped.insert(record.run);
    }
    match runners.stray_runs() {
        Ok(stray) => {
            for run in stray.difference(&reaped) {
                let teardown = runners.teardown(run);
                let line = format!("reaped stray run {run}: {}", teardown.summary());
                eprintln!("bosn runners: {line}");
                lines.push(line);
            }
        }
        Err(error) => eprintln!("bosn runners: stray-run sweep skipped: {error}"),
    }
    runners.clear_ledger();
    lines
}
