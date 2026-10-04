//! The isolated engine act runs against.
//!
//! #349's owned engine ([`crate::act_engine`]) freezes its creation profile
//! before creating the pinned Docker image with a read-only root, private
//! storage/cgroup namespace and no host path or socket. Its verified machine
//! cache mount survives; everything else goes with the engine on retirement.
//!
//! [`DockerActBackend`] runs act through the nested engine's private socket.
//! [`super::pins`] verifies the act archive/binary and runner manifest/config.
//! Frozen source and event payloads are streamed in; completed tool-cache
//! installs are seeded and saved through the machine-wide volume.

use std::{collections::BTreeMap, future::Future, path::Path, pin::Pin, time::Duration};

use bosn_engine::{CommandError, DockerEngine, EngineEvent, RunOptions};
use bosn_registry::act::{
    ActEngineCacheVolume, ActEngineIntent, ActEngineObservation, ActEngineRecord,
};
use kernal_api::async_engine::{self, CancellationToken};

use crate::{RegistryActor, act_engine};

#[cfg(all(test, unix))]
mod cache_staging_tests;
mod cache_usage;
mod control;
pub use cache_usage::HelperCleanupRetry;
#[cfg(all(test, unix))]
mod cache_usage_transport_tests;
mod inventory;
mod legacy_lease;
mod machine_policy;
mod maintenance;
#[cfg(test)]
mod maintenance_contention_tests;
mod maintenance_helper;
mod maintenance_lease;
mod maintenance_loop;
mod maintenance_reporting;
mod migration;
pub use inventory::InventoryAttempt;
pub use maintenance::MaintenanceAttempt;
pub use maintenance_helper::MaintenanceHelperAttempt;
pub use maintenance_loop::MaintenanceTick;
pub use migration::ImportAttempt;
mod act_install;
mod readiness;
mod tool_recovery;
use act_install::act_archive;
pub(crate) use act_install::install_act_script;
mod lines;
mod runner_tools;
mod toolcache;
use lines::LineBuffer;
#[cfg(test)]
use lines::MAX_LINE;
use toolcache::{prepare_toolcache_script, save_toolcache_script};

pub(crate) fn tool_overlay_recipe_sha256() -> String {
    toolcache::overlay_recipe_sha256()
}

pub use super::pins::{ACT_VERSION, ActArtifact, RUNNER_IMAGE, act_artifact, runner_tag};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The pinned engine image, by its publisher manifest digest.
pub fn engine_image() -> String {
    format!("docker.io/library/docker@{}", act_engine::ENGINE_MANIFEST)
}
/// Where the machine-wide cache volume is mounted in the engine.
pub const ENGINE_CACHE: &str = "/bosn/cache";
/// The engine's root is read-only: everything a run writes lives under its
/// private (executable) storage tmpfs, next to the nested daemon's data.
pub const ENGINE_WORK: &str = "/var/lib/docker/bosn-ci";

/// The machine-wide CI cache: a bosn-labelled named volume holding the act
/// release, the runner image tar, action checkouts, the tool cache and the
/// per-repository act cache-server stores. A volume (not a host directory)
/// because the privileged engine writes as root, and Docker Desktop shares
/// no paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheVolume {
    pub name: String,
    /// The labels it is created with (its owner's; verified before use).
    pub labels: BTreeMap<String, String>,
}

pub const CACHE_VOLUME: &str = "bosn-ci-cache-v1";

impl CacheVolume {
    /// The machine-wide cache volume, labelled as owned by `registry`.
    pub fn machine(registry: &str, created: f64) -> Result<Self, String> {
        Ok(Self {
            name: CACHE_VOLUME.into(),
            labels: act_engine::cache_volume_labels(registry, created)
                .map_err(|error| error.to_string())?,
        })
    }

    /// The mount frozen into an engine's creation profile.
    pub fn mount(&self) -> ActEngineCacheVolume {
        ActEngineCacheVolume {
            name: self.name.clone(),
            target: ENGINE_CACHE.into(),
        }
    }
}

/// Secret environment for act. `Debug` prints names only.
#[derive(Clone, Default)]
pub struct SecretEnv(pub Vec<(String, String)>);
impl std::fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(k, _)| k))
            .finish()
    }
}

/// The act command, built only from validated semantic fields.
#[derive(Clone, Debug)]
pub struct ActInvocation {
    pub event: String,
    pub workflow: String,
    /// bosn rewrote the workflow, so act runs the overlay's copy (#424).
    pub workflow_overlaid: bool,
    pub job: Option<String>,
    /// Typed repository route; cohort selection requires verified enrollment.
    pub cache_route: super::cache_cohort::CacheRoute,
    /// Passed to act as `-s NAME`; values travel only in the docker client's
    /// environment (`exec --env NAME`), never in argv.
    pub secrets: SecretEnv,
    /// act `--input`, `--matrix` and `--env` (#430), validated at submit.
    pub params: super::params::RunParams,
}

/// The `runs-on` labels act runs locally, all on the pinned runner image;
/// any other is unsupported ([`super::matrix_runner`] decides it per matrix
/// leg).
pub const LOCAL_RUNNER_LABELS: [&str; 3] = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"];

impl ActInvocation {
    /// The workflow act plans: bosn's rewrite when there is one. The
    /// workspace jobs check out always holds the original (#424).
    pub fn workflow_arg(&self) -> String {
        if self.workflow_overlaid {
            format!("{ENGINE_WORK}/overlay/{}", self.workflow)
        } else {
            self.workflow.clone()
        }
    }

    /// Arguments after `act`. Platform mappings cover the Linux labels; any
    /// other `runs-on` is reported unsupported by act and never passes.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            self.event.clone(),
            "-W".into(),
            self.workflow_arg(),
            // Local reusable workflows and composite actions bosn rewrote.
            "--workflow-overlay".into(),
            format!("{ENGINE_WORK}/overlay"),
            "--eventpath".into(),
            format!("{ENGINE_WORK}/event.json"),
            "--json".into(),
            "--pull=false".into(),
            "--action-cache-path".into(),
            format!("{ENGINE_CACHE}/actions"),
            // Legacy in-place checkouts race between concurrent runs
            // (zackees/clud#1724); the new cache extracts per run.
            "--use-new-action-cache".into(),
            // Per engine, so concurrent runs never share artifacts or ports.
            "--artifact-server-path".into(),
            format!("{ENGINE_WORK}/artifacts"),
        ];
        args.extend(self.cache_route.args());
        args.extend(["--env".into(), runner_tools::path_env()]);
        let runner = runner_tag();
        for label in LOCAL_RUNNER_LABELS {
            args.push("-P".into());
            args.push(format!("{label}={runner}"));
        }
        for (name, _) in &self.secrets.0 {
            args.push("-s".into());
            args.push(name.clone());
        }
        if let Some(job) = &self.job {
            args.push("-j".into());
            args.push(job.clone());
        }
        args.extend(self.params.act_args());
        args
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEnd {
    Exited(i32),
    TimedOut,
    Cancelled,
}

/// One line of engine output, already split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineLine {
    Stdout(String),
    Stderr(String),
}

/// The trusted engine runtime behind `bosn ci`. Engine identity, ownership
/// and removal are the owned-engine layer's ([`crate::act_engine`],
/// [`crate::act_runtime::retire_engine`]); the in-engine steps address an
/// engine by its immutable ID, which the caller only holds while its
/// execution claim is verified.
pub trait ActEngineBackend: Send + Sync {
    /// Make the pinned engine image present on the host engine (pulled by
    /// its manifest digest when missing).
    fn ensure_engine_image(&self) -> BoxFuture<'_, Result<(), String>>;
    /// What the host engine's machine offers, for sizing a new engine
    /// ([`super::limits::size_engine`]).
    fn host_resources(&self) -> BoxFuture<'_, Result<super::limits::HostResources, String>>;
    /// Create the machine-wide cache volume unless it exists.
    fn ensure_cache<'a>(&'a self, cache: &'a CacheVolume) -> BoxFuture<'a, Result<(), String>>;
    /// Commit `intent`, then create, verify, register and start its engine.
    /// A failure after the intent commits leaves the record for cleanup.
    fn create<'a>(
        &'a self,
        registry: &'a RegistryActor,
        intent: &'a ActEngineIntent,
        owner: &'a str,
        at: f64,
    ) -> BoxFuture<'a, Result<ActEngineObservation, String>>;
    /// Retire a `cleanup_required` engine: prove it absent or remove exactly
    /// the committed engine, then record the terminal receipt.
    fn retire<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        record: &'a ActEngineRecord,
        budget: Duration,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// What every run's engine needs, whichever run it serves: wait for the
    /// nested engine, install act and load the runner image (each proven to
    /// be its pin). Only bosn's own fixed scripts run, so a spare engine
    /// (#410) is prepared this far before any run claims it. The artifact
    /// must be the one the intent recorded.
    fn prepare_engine<'a>(
        &'a self,
        engine: &'a str,
        act: ActArtifact,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// What one run needs on a prepared engine: seed act's tool cache from
    /// the machine-wide store as of now, and stream in the frozen source and
    /// event payload.
    fn prepare_run<'a>(
        &'a self,
        engine: &'a str,
        source: &'a Path,
        event: &'a Path,
        generation: Option<&'a bosn_registry::act::ActToolGenerationBinding>,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// `act -l` for the workflow (declared jobs and their stages).
    fn list<'a>(
        &'a self,
        engine: &'a str,
        workflow: &'a str,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn execute<'a>(
        &'a self,
        engine: &'a str,
        invocation: &'a ActInvocation,
        deadline: Duration,
        cancellation: &'a CancellationToken,
        lines: &'a async_engine::Sender<EngineLine>,
    ) -> BoxFuture<'a, Result<ExecEnd, String>>;
    /// How full the engine's private storage is now ([`super::storage`]).
    fn storage_usage<'a>(
        &'a self,
        engine: &'a str,
    ) -> BoxFuture<'a, Result<super::storage::StorageUsage, String>>;
    /// Save the tool-cache installs this run completed (act's
    /// `/opt/hostedtoolcache`) into the machine-wide cache, each atomically;
    /// best-effort, before the engine is removed.
    fn save_toolcache<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// The cache volume's size in bytes; `None` when it does not exist.
    fn cache_usage<'a>(
        &'a self,
        volume: &'a str,
    ) -> BoxFuture<'a, Result<super::CacheUsage, String>>;
    /// Service accounting commits a durable intent before creating its helper.
    fn tracked_cache_usage<'a>(
        &'a self,
        volume: &'a str,
        registry: &'a RegistryActor,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<super::CacheUsage, String>> {
        let _ = (registry, owner);
        self.cache_usage(volume)
    }
    /// Retry one inactive durable helper; implementations without Docker helpers are no-ops.
    fn retry_cache_helpers<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        cursor: Option<String>,
    ) -> BoxFuture<'a, Result<HelperCleanupRetry, String>> {
        let _ = (registry, owner, cursor);
        Box::pin(async { Ok(HelperCleanupRetry::default()) })
    }
    /// Maintain only an existing cohort with an agreed shared machine policy.
    /// Backends without shared stores have no background maintenance work.
    fn maintain_existing_cohort<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        stop: &'a CancellationToken,
    ) -> BoxFuture<'a, ()> {
        let _ = (registry, owner, stop);
        Box::pin(async {})
    }
    /// Remove the cache volume. The host engine refuses while any container
    /// (another daemon's run included) still uses it.
    fn remove_cache<'a>(&'a self, volume: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

const CONTROL_DEADLINE: Duration = Duration::from_secs(60);
/// A storage sample holds up the run's log drain; it is short or skipped.
const SAMPLE_DEADLINE: Duration = Duration::from_secs(10);
const PULL_DEADLINE: Duration = Duration::from_secs(30 * 60);
const CONTROL_OUTPUT: usize = 1024 * 1024;
const RUN_OUTPUT: usize = 1024 * 1024 * 1024;

pub struct DockerActBackend {
    docker: DockerEngine,
    active_helpers: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl Default for DockerActBackend {
    fn default() -> Self {
        Self::new(DockerEngine::docker())
    }
}

impl DockerActBackend {
    pub fn new(docker: DockerEngine) -> Self {
        Self {
            docker,
            active_helpers: Default::default(),
        }
    }

    async fn checked(
        &self,
        what: &str,
        args: Vec<String>,
        deadline: Duration,
    ) -> Result<String, String> {
        let result = self.run(args, deadline).await?;
        if !result.ok() {
            return Err(format!(
                "{what} failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&result.stdout).trim().to_string())
    }

    async fn volume_exists(&self, volume: &str) -> Result<bool, String> {
        let result = self
            .run(owned(&["volume", "inspect", volume]), CONTROL_DEADLINE)
            .await?;
        cache_usage::volume_present(result.ok(), &result.stderr)
    }

    /// Install the pinned act release, verified by sha256, from the cache
    /// volume (downloading it there once).
    async fn install_act(&self, engine: &str, act: ActArtifact) -> Result<(), String> {
        let arch = self
            .checked(
                "engine architecture",
                Self::exec(engine, "uname -m"),
                CONTROL_DEADLINE,
            )
            .await?;
        if act_artifact(&arch) != Some(act) {
            return Err(format!(
                "engine architecture {arch} does not match the recorded act artifact"
            ));
        }
        let version = self
            .checked(
                "act install",
                Self::exec(engine, &install_act_script(act)),
                PULL_DEADLINE,
            )
            .await?;
        if !version.ends_with(ACT_VERSION) {
            return Err(format!(
                "installed act reports {version:?}, expected {ACT_VERSION}"
            ));
        }
        Ok(())
    }

    /// Stream `file` into the engine on `docker exec`'s stdin. The engine's
    /// root is read-only and its storage is a private tmpfs, which `docker
    /// cp` cannot reach, so inputs travel the way the owned runtime's do.
    async fn stream_in(
        &self,
        what: &str,
        engine: &str,
        file: &Path,
        script: &str,
    ) -> Result<(), String> {
        let input = std::fs::File::open(file).map_err(|e| format!("{what}: {e}"))?;
        let size = input.metadata().map_err(|e| format!("{what}: {e}"))?.len();
        let result = self
            .docker
            .with_args(Self::exec_stdin(engine, script))
            .capture_with_stdin_file_async(
                input,
                size,
                RunOptions::bounded(PULL_DEADLINE, CONTROL_OUTPUT),
                None,
            )
            .await
            .map_err(|e| format!("{what}: {e}"))?;
        if !result.ok() {
            return Err(format!(
                "{what} failed: {}",
                String::from_utf8_lossy(&result.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Stream the frozen source and bosn's overlay (each as a tar) and the
    /// event payload into the engine's work directory.
    async fn copy_inputs(&self, engine: &str, source: &Path, event: &Path) -> Result<(), String> {
        self.copy_tree(engine, source, "src").await?;
        let overlay = super::store::overlay_beside(source);
        if overlay.is_dir() {
            self.copy_tree(engine, &overlay, "overlay").await?;
        }
        self.stream_in(
            "event copy",
            engine,
            event,
            &format!("cat > {ENGINE_WORK}/event.json"),
        )
        .await
    }

    /// Stream the host directory `tree` into `{ENGINE_WORK}/<into>`.
    async fn copy_tree(&self, engine: &str, tree: &Path, into: &str) -> Result<(), String> {
        let archive = tree.with_extension("engine.tar");
        let (from, to) = (tree.to_path_buf(), archive.clone());
        let tar = async_engine::launch_blocking(move || {
            kernal_api::run_bounded_command(
                kernal_api::SpawnSpec::new("tar")
                    .arg("-C")
                    .arg(&from)
                    .arg("-cf")
                    .arg(&to)
                    .arg(".")
                    .stdin(kernal_api::StreamMode::Null)
                    .stdout(kernal_api::StreamMode::Piped)
                    .stderr(kernal_api::StreamMode::Piped),
                PULL_DEADLINE,
                CONTROL_OUTPUT,
            )
        })
        .await
        .map_err(|e| format!("{into} archive: {e}"))?
        .map_err(|e| format!("{into} archive: {e}"))?;
        if tar.exit.raw_code() != 0 {
            let _ = std::fs::remove_file(&archive);
            return Err(format!(
                "{into} archive failed: {}",
                String::from_utf8_lossy(&tar.stderr).trim()
            ));
        }
        let copied = self
            .stream_in(
                &format!("{into} copy"),
                engine,
                &archive,
                &format!("tar -xf - -C {ENGINE_WORK}/{into}"),
            )
            .await;
        let _ = std::fs::remove_file(&archive);
        copied
    }

    /// Make [`runner_tag`] present in the engine, proven to be the pinned
    /// runner ([`super::pins::verify_runner`]): loaded from the cache
    /// volume's image tar, or pulled by digest once and saved there. A tar
    /// whose image fails the proof is discarded and the image pulled again.
    async fn load_runner(&self, engine: &str) -> Result<(), String> {
        let mut refused = None;
        for script in [load_runner_script(), reload_runner_script()] {
            self.checked("runner image", Self::exec(engine, &script), PULL_DEADLINE)
                .await?;
            let inspect = self
                .checked(
                    "runner image inspect",
                    owned(&["exec", engine, "docker", "image", "inspect", &runner_tag()]),
                    CONTROL_DEADLINE,
                )
                .await?;
            match super::pins::verify_runner(inspect.as_bytes()) {
                Ok(_) => return Ok(()),
                Err(error) => refused = Some(error),
            }
        }
        Err(refused.unwrap_or_default())
    }

    fn exec(engine: &str, script: &str) -> Vec<String> {
        ["exec", engine, "sh", "-ec", script]
            .into_iter()
            .map(String::from)
            .collect()
    }

    fn exec_stdin(engine: &str, script: &str) -> Vec<String> {
        ["exec", "-i", engine, "sh", "-ec", script]
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// `docker exec` arguments that run act in the work tree, with a
    /// writable home on the engine's storage (its root is read-only).
    fn act_exec(
        engine: &str,
        secrets: &SecretEnv,
        route: Option<&super::cache_cohort::CacheRoute>,
    ) -> Vec<String> {
        let mut args = owned(&["exec", "-w"]);
        args.push(format!("{ENGINE_WORK}/src"));
        for (key, value) in [
            ("HOME", "home"),
            ("XDG_CACHE_HOME", "home/.cache"),
            ("XDG_CONFIG_HOME", "home/.config"),
            ("TMPDIR", "tmp"),
        ] {
            args.push("--env".into());
            args.push(format!("{key}={ENGINE_WORK}/{value}"));
        }
        // Secret values reach act only through the docker client's own
        // environment (`--env NAME` copies it); never through argv.
        for (key, _) in &secrets.0 {
            args.push("--env".into());
            args.push(key.clone());
        }
        args.push(engine.into());
        let legacy = match route {
            Some(super::cache_cohort::CacheRoute::Legacy(namespace)) => Some(namespace),
            _ => None,
        };
        args.extend(legacy_lease::command(legacy));
        args
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

impl ActEngineBackend for DockerActBackend {
    fn ensure_engine_image(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let image = engine_image();
            let inspect = owned(&["image", "inspect", "--format", "{{.Id}}", &image]);
            if self.run(inspect, CONTROL_DEADLINE).await?.ok() {
                return Ok(());
            }
            self.checked(
                "engine image pull",
                owned(&["pull", "-q", &image]),
                PULL_DEADLINE,
            )
            .await
            .map(|_| ())
        })
    }

    fn host_resources(&self) -> BoxFuture<'_, Result<super::limits::HostResources, String>> {
        Box::pin(async move {
            let info = self
                .checked(
                    "docker info",
                    owned(&[
                        "info",
                        "--format",
                        "{{.MemTotal}} {{.NCPU}} {{.DockerRootDir}}",
                    ]),
                    CONTROL_DEADLINE,
                )
                .await?;
            let meminfo = std::fs::read_to_string("/proc/meminfo").ok();
            super::limits::HostResources::parse(&info, meminfo.as_deref(), |root| {
                kernal_api::resources_available_space(std::path::Path::new(root)).ok()
            })
        })
    }

    fn create<'a>(
        &'a self,
        registry: &'a RegistryActor,
        intent: &'a ActEngineIntent,
        owner: &'a str,
        at: f64,
    ) -> BoxFuture<'a, Result<ActEngineObservation, String>> {
        Box::pin(async move {
            let limits = act_engine::frozen_limits(intent).map_err(|e| e.to_string())?;
            let proofs = act_engine::bundled_engine_manifests().map_err(|e| e.to_string())?;
            let proof = proofs
                .get(&intent.engine_image_digest)
                .ok_or("no publisher proof for the engine image")?;
            act_engine::create_owned_engine_from_manifest(
                registry,
                &self.docker,
                intent.clone(),
                owner,
                proof,
                limits,
                at,
            )
            .await
            .map_err(|e| format!("engine create: {e}"))
        })
    }

    fn retire<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        record: &'a ActEngineRecord,
        budget: Duration,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let proofs = act_engine::bundled_engine_manifests().map_err(|e| e.to_string())?;
            let never = async_engine::CancellationSource::new();
            crate::act_runtime::retire_engine(
                registry,
                &self.docker,
                owner,
                &proofs,
                record,
                budget,
                &never.token(),
            )
            .await
            .map_err(|e| e.to_string())
        })
    }

    fn storage_usage<'a>(
        &'a self,
        engine: &'a str,
    ) -> BoxFuture<'a, Result<super::storage::StorageUsage, String>> {
        Box::pin(async move {
            let script = format!("df -Pk {}", super::storage::STORAGE_PATH);
            let df = self
                .checked(
                    "storage usage",
                    Self::exec(engine, &script),
                    SAMPLE_DEADLINE,
                )
                .await?;
            super::storage::StorageUsage::parse_df(&df)
        })
    }

    fn save_toolcache<'a>(&'a self, engine: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.checked(
                "tool cache save",
                Self::exec(engine, &save_toolcache_script()),
                PULL_DEADLINE,
            )
            .await
            .map(|_| ())
        })
    }

    fn cache_usage<'a>(
        &'a self,
        volume: &'a str,
    ) -> BoxFuture<'a, Result<super::CacheUsage, String>> {
        Box::pin(self.measure_cache(volume))
    }

    fn tracked_cache_usage<'a>(
        &'a self,
        volume: &'a str,
        registry: &'a RegistryActor,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<super::CacheUsage, String>> {
        Box::pin(self.measure_cache_tracked(volume, Some((registry, owner))))
    }
    fn retry_cache_helpers<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        cursor: Option<String>,
    ) -> BoxFuture<'a, Result<HelperCleanupRetry, String>> {
        Box::pin(self.retry_measurements(registry, owner, cursor))
    }
    fn maintain_existing_cohort<'a>(
        &'a self,
        registry: &'a RegistryActor,
        owner: &'a str,
        stop: &'a CancellationToken,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.supervise_existing_cohort(registry, owner, stop))
    }
    fn remove_cache<'a>(&'a self, volume: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if !self.volume_exists(volume).await? {
                return Ok(());
            }
            self.checked(
                "cache volume remove",
                owned(&["volume", "rm", volume]),
                CONTROL_DEADLINE,
            )
            .await
            .map(|_| ())
        })
    }

    fn ensure_cache<'a>(&'a self, cache: &'a CacheVolume) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.volume_exists(&cache.name).await? {
                return Ok(());
            }
            let mut args = owned(&["volume", "create"]);
            for (key, value) in &cache.labels {
                args.push("--label".into());
                args.push(format!("{key}={value}"));
            }
            args.push(cache.name.clone());
            self.checked("cache volume create", args, CONTROL_DEADLINE)
                .await
                .map(|_| ())
        })
    }

    fn prepare_engine<'a>(
        &'a self,
        engine: &'a str,
        act: ActArtifact,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.wait_ready(engine).await?;
            self.checked(
                "work directory",
                Self::exec(engine, &work_dirs_script()),
                CONTROL_DEADLINE,
            )
            .await?;
            self.install_act(engine, act).await?;
            self.load_runner(engine).await
        })
    }

    fn prepare_run<'a>(
        &'a self,
        engine: &'a str,
        source: &'a Path,
        event: &'a Path,
        generation: Option<&'a bosn_registry::act::ActToolGenerationBinding>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.checked(
                "tool cache seed",
                Self::exec(engine, &prepare_toolcache_script(generation)?),
                PULL_DEADLINE,
            )
            .await?;
            self.checked(
                "runner stock tools",
                Self::exec(engine, &runner_tools::prepare_script()),
                PULL_DEADLINE,
            )
            .await?;
            self.copy_inputs(engine, source, event).await
        })
    }

    fn list<'a>(
        &'a self,
        engine: &'a str,
        workflow: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut args = Self::act_exec(engine, &SecretEnv::default(), None);
            args.extend(owned(&["-l", "-W", workflow, "--workflow-overlay"]));
            args.push(format!("{ENGINE_WORK}/overlay"));
            self.checked("act -l", args, CONTROL_DEADLINE).await
        })
    }

    fn execute<'a>(
        &'a self,
        engine: &'a str,
        invocation: &'a ActInvocation,
        deadline: Duration,
        cancellation: &'a CancellationToken,
        lines: &'a async_engine::Sender<EngineLine>,
    ) -> BoxFuture<'a, Result<ExecEnd, String>> {
        Box::pin(async move {
            if let super::cache_cohort::CacheRoute::Cohort { policy, .. } = invocation.cache_route {
                self.agree_cache_policy(engine, policy).await?;
            }
            let mut args =
                Self::act_exec(engine, &invocation.secrets, Some(&invocation.cache_route));
            args.extend(invocation.args());
            let (events, mut receiver) = async_engine::channel(256);
            let docker = invocation
                .secrets
                .0
                .iter()
                .fold(self.docker.with_args(args), |docker, (key, value)| {
                    docker.env(key, value)
                });
            let forward = async {
                let mut stdout = LineBuffer::default();
                let mut stderr = LineBuffer::default();
                while let Some(event) = receiver.recv().await {
                    let (buffer, make): (&mut LineBuffer, fn(String) -> EngineLine) = match event {
                        EngineEvent::Stdout(bytes) => {
                            stdout.push(&bytes);
                            (&mut stdout, EngineLine::Stdout)
                        }
                        EngineEvent::Stderr(bytes) => {
                            stderr.push(&bytes);
                            (&mut stderr, EngineLine::Stderr)
                        }
                    };
                    for line in buffer.drain_lines() {
                        if lines.send(make(line)).await.is_err() {
                            return;
                        }
                    }
                }
                for line in stdout.finish() {
                    let _ = lines.send(EngineLine::Stdout(line)).await;
                }
                for line in stderr.finish() {
                    let _ = lines.send(EngineLine::Stderr(line)).await;
                }
            };
            let stream = async {
                let result = docker
                    .stream(
                        RunOptions::streaming(deadline, RUN_OUTPUT),
                        Some(cancellation),
                        &events,
                    )
                    .await;
                drop(events);
                result
            };
            let (result, ()) = async_engine::join(stream, forward).await;
            // Ending the docker client does not stop act inside the engine;
            // the caller removes the whole engine, which does.
            match result {
                Ok(result) => Ok(ExecEnd::Exited(result.exit_code)),
                Err(CommandError::Deadline { .. }) => Ok(ExecEnd::TimedOut),
                Err(CommandError::Cancelled { .. }) => Ok(ExecEnd::Cancelled),
                Err(error) => Err(error.to_string()),
            }
        })
    }
}

/// The run's work tree on the engine's private storage.
fn work_dirs_script() -> String {
    format!(
        "mkdir -p {ENGINE_WORK}/bin {ENGINE_WORK}/src {ENGINE_WORK}/overlay {ENGINE_WORK}/artifacts \
         {ENGINE_WORK}/home/.cache {ENGINE_WORK}/home/.config {ENGINE_WORK}/tmp"
    )
}

/// The runner image tar in the cache volume.
fn runner_tar() -> String {
    format!(
        "{ENGINE_CACHE}/images/{}.tar",
        runner_tag().replace([':', '/'], "-")
    )
}

/// Shell that loads the runner image tar from the cache volume, or pulls
/// the pinned image, tags it [`runner_tag`] and saves the tar atomically.
fn load_runner_script() -> String {
    format!("{} {}", runner_input_lock(true), load_runner_body())
}

fn runner_input_lock(shared: bool) -> String {
    format!(
        "mkdir -p {ENGINE_CACHE}/images; exec 9>>{}.lock; flock -{} 9;",
        runner_tar(),
        if shared { "s" } else { "x" }
    )
}

fn load_runner_body() -> String {
    let tag = runner_tag();
    let tar = runner_tar();
    let restore = "[ -f \"$tar\" ] && docker load -q -i \"$tar\" >/dev/null";
    format!(
        "tar={tar}; mkdir -p {ENGINE_CACHE}/images; \
         if ! {{ {restore}; }}; then \
           flock -x 9; \
           if ! {{ {restore}; }}; then \
           docker pull -q --platform linux/amd64 {RUNNER_IMAGE} >/dev/null && \
           docker tag {RUNNER_IMAGE} {tag} && \
           stage=$(mktemp \"$tar.XXXXXXXX\") && \
           trap 'rm -f \"$stage\"' EXIT && \
           docker save --platform linux/amd64 -o \"$stage\" {tag} && mv \"$stage\" \"$tar\" || exit 1; \
           fi; \
         fi; \
         docker image inspect {tag} >/dev/null"
    )
}

/// After a cached tar failed the runner proof: drop it and the image it
/// loaded, then pull and save again.
fn reload_runner_script() -> String {
    format!(
        "{lock} rm -f {tar}; docker image rm -f {tag} >/dev/null 2>&1 || :; {load}",
        lock = runner_input_lock(false),
        tar = runner_tar(),
        tag = runner_tag(),
        load = load_runner_body(),
    )
}

#[cfg(test)]
mod tests;
