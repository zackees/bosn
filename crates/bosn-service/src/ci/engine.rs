//! The isolated engine act runs against.
//!
//! [`DockerActBackend`] creates one privileged `docker:dind` container per run
//! on the host engine, pinned by digest and labelled with the registry's
//! ownership labels. act itself runs *inside* that container through
//! `docker exec`, so it only ever sees the nested engine's private socket
//! (`unix:///var/run/docker.sock` inside the engine); the host socket is never
//! mounted. Every container, network, volume and image act creates lives in
//! the nested engine's storage, an anonymous volume removed with the engine
//! (`docker rm -f -v`), which makes the engine the cleanup boundary.
//!
//! The act binary is fetched inside the engine from the pinned release URL
//! and checked against its pinned sha256 before use.

use std::{collections::BTreeMap, future::Future, path::Path, pin::Pin, time::Duration};

use bosn_engine::{CommandError, DockerEngine, EngineEvent, RunOptions};
use bosn_registry::act::ActEngineObservation;
use kernal_api::async_engine::{self, CancellationToken};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// docker:28-dind (multi-arch index), pinned by digest.
pub const ENGINE_IMAGE: &str =
    "docker@sha256:2a232a42256f70d78e3cc5d2b5d6b3276710a0de0596c145f627ecfae90282ac";
/// catthehacker/ubuntu:act-24.04, pinned by digest; maps `ubuntu-*` runners.
pub const RUNNER_IMAGE: &str = "catthehacker/ubuntu:act-24.04@sha256:c58e2b364da03b0c804c7d660f2ecbedf2f221a382b9baa0b344b0144780ff43";
pub const ACT_VERSION: &str = "0.2.88";
/// Where the daemon's persistent CI cache directory appears in the engine.
pub const ENGINE_CACHE: &str = "/bosn/cache";

/// The runner image's engine-local name. The pinned image is loaded from the
/// daemon's image cache under this tag (a digest reference cannot be saved
/// and loaded portably), and act's platform mappings name it.
pub fn runner_tag() -> String {
    let digest = RUNNER_IMAGE.rsplit_once("@sha256:").map_or("", |(_, d)| d);
    format!("bosn/act-runner:{}", &digest[..12.min(digest.len())])
}

/// One pinned act release artifact for an engine architecture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActArtifact {
    pub url: &'static str,
    pub sha256: &'static str,
}

/// The pinned act artifact for the engine's architecture (`docker info`).
pub fn act_artifact(architecture: &str) -> Option<ActArtifact> {
    match architecture {
        "x86_64" | "amd64" => Some(ActArtifact {
            url: "https://github.com/nektos/act/releases/download/v0.2.88/act_Linux_x86_64.tar.gz",
            sha256: "1eb9996682dfcc053ac8f3f90f2ec50376f0cdfc229712d82da03d673c63a2b3",
        }),
        "aarch64" | "arm64" => Some(ActArtifact {
            url: "https://github.com/nektos/act/releases/download/v0.2.88/act_Linux_arm64.tar.gz",
            sha256: "94d87738f7ea6650782c8505366c758c99db54cc67bd8c711583478c93305d78",
        }),
        _ => None,
    }
}

/// Everything the backend needs to create one engine.
#[derive(Clone, Debug)]
pub struct EngineSpec {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    /// The machine-wide cache volume, mounted at [`ENGINE_CACHE`].
    pub cache: CacheVolume,
}

/// The machine-wide CI cache: a bosn-labelled named volume holding the act
/// release, the runner image tar, action checkouts and the per-repository
/// act cache-server stores. A volume (not a host directory) because the
/// privileged engine writes as root, and Docker Desktop shares no paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheVolume {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

pub const CACHE_VOLUME: &str = "bosn-ci-cache-v1";

impl CacheVolume {
    /// The machine-wide cache volume, labelled as owned by `registry`.
    pub fn machine(registry: &str, created: f64) -> Result<Self, String> {
        let labels = bosn_core::ResourceLabels::new(
            registry,
            bosn_core::ResourceKind::Volume,
            "ci-cache",
            "v1",
            bosn_core::Scope::Machine,
            "machine",
            &created.to_string(),
            Some(bosn_core::Retention::Pinned),
        )
        .map_err(|_| "cache volume labels".to_string())?
        .to_map()
        .into_iter()
        .map(|(k, v)| (k.into(), v))
        .collect();
        Ok(Self {
            name: CACHE_VOLUME.into(),
            labels,
        })
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
    pub job: Option<String>,
    /// Repository identity (hex) that namespaces the act cache server store,
    /// so two repositories' `actions/cache` keys never meet.
    pub cache_namespace: String,
    /// Passed to act as `-s NAME`; values travel only in the docker client's
    /// environment (`exec --env NAME`), never in argv.
    pub secrets: SecretEnv,
}

impl ActInvocation {
    /// Arguments after `act`. Platform mappings cover the Linux labels; any
    /// other `runs-on` is reported unsupported by act and never passes.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            self.event.clone(),
            "-W".into(),
            self.workflow.clone(),
            "--eventpath".into(),
            "/bosn/event.json".into(),
            "--json".into(),
            "--pull=false".into(),
            "--action-cache-path".into(),
            format!("{ENGINE_CACHE}/actions"),
            "--cache-server-path".into(),
            format!("{ENGINE_CACHE}/actcache/{}", self.cache_namespace),
            // Per engine, so concurrent runs never share artifacts or ports.
            "--artifact-server-path".into(),
            "/bosn/artifacts".into(),
        ];
        let runner = runner_tag();
        for label in ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04"] {
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

/// The trusted engine runtime. Inspect results are the only source of
/// [`ActEngineObservation`]s; `Ok(None)` from [`Self::inspect`] is a proven
/// absence, while `Err` means the engine state is unknown.
pub trait ActEngineBackend: Send + Sync {
    /// Resolve the immutable local image ID of [`ENGINE_IMAGE`], pulling it
    /// when missing. Recorded in the intent before creation.
    fn resolve_engine_image(&self) -> BoxFuture<'_, Result<String, String>>;
    /// Create the machine-wide cache volume unless it exists.
    fn ensure_cache<'a>(&'a self, cache: &'a CacheVolume) -> BoxFuture<'a, Result<(), String>>;
    fn create<'a>(&'a self, spec: &'a EngineSpec) -> BoxFuture<'a, Result<(), String>>;
    fn inspect<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ActEngineObservation>, String>>;
    /// Wait for the nested engine, install act, copy the frozen source and
    /// event payload, and pull the runner image inside the engine.
    /// The engine's architecture must match `act`, the artifact the intent
    /// recorded; otherwise this fails rather than install another binary.
    fn prepare<'a>(
        &'a self,
        name: &'a str,
        source: &'a Path,
        event: &'a Path,
        act: ActArtifact,
    ) -> BoxFuture<'a, Result<(), String>>;
    /// `act -l` for the workflow (declared jobs and their stages).
    fn list<'a>(
        &'a self,
        name: &'a str,
        workflow: &'a str,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn execute<'a>(
        &'a self,
        name: &'a str,
        invocation: &'a ActInvocation,
        deadline: Duration,
        cancellation: &'a CancellationToken,
        lines: &'a async_engine::Sender<EngineLine>,
    ) -> BoxFuture<'a, Result<ExecEnd, String>>;
    /// Remove the engine with this exact immutable ID (never by name: the
    /// name could be taken by something else after authorization).
    fn remove<'a>(&'a self, engine_id: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// The cache volume's size in bytes; `None` when it does not exist.
    fn cache_bytes<'a>(&'a self, volume: &'a str) -> BoxFuture<'a, Result<Option<u64>, String>>;
    /// Remove the cache volume. The host engine refuses while any container
    /// (another daemon's run included) still uses it.
    fn remove_cache<'a>(&'a self, volume: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

const CONTROL_DEADLINE: Duration = Duration::from_secs(60);
const PULL_DEADLINE: Duration = Duration::from_secs(30 * 60);
const CONTROL_OUTPUT: usize = 1024 * 1024;
const RUN_OUTPUT: usize = 1024 * 1024 * 1024;

pub struct DockerActBackend {
    docker: DockerEngine,
}

impl Default for DockerActBackend {
    fn default() -> Self {
        Self::new(DockerEngine::docker())
    }
}

impl DockerActBackend {
    pub fn new(docker: DockerEngine) -> Self {
        Self { docker }
    }

    async fn run(
        &self,
        args: Vec<String>,
        deadline: Duration,
    ) -> Result<bosn_engine::CommandResult, String> {
        self.docker
            .with_args(args)
            .capture_async(RunOptions::bounded(deadline, CONTROL_OUTPUT))
            .await
            .map_err(|e| e.to_string())
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
        Ok(self
            .run(owned(&["volume", "inspect", volume]), CONTROL_DEADLINE)
            .await?
            .ok())
    }

    async fn wait_ready(&self, name: &str) -> Result<(), String> {
        let deadline = async_engine::Deadline::after(Duration::from_secs(60));
        loop {
            let probe = Self::exec(name, "docker info >/dev/null 2>&1");
            if self.run(probe, CONTROL_DEADLINE).await?.ok() {
                return Ok(());
            }
            if deadline.is_elapsed() {
                return Err("nested engine did not become ready within 60s".into());
            }
            async_engine::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Install the pinned act release, verified by sha256, from the cache
    /// volume (downloading it there once).
    async fn install_act(&self, name: &str, act: ActArtifact) -> Result<(), String> {
        let arch = self
            .checked(
                "engine architecture",
                Self::exec(name, "uname -m"),
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
                Self::exec(name, &install_act_script(act)),
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

    /// Copy the frozen source to `/src` and the event payload beside it.
    async fn copy_inputs(&self, name: &str, source: &Path, event: &Path) -> Result<(), String> {
        self.checked(
            "source copy",
            owned(&["exec", name, "mkdir", "-p", "/src"]),
            CONTROL_DEADLINE,
        )
        .await?;
        let copy = vec![
            "cp".into(),
            format!("{}/.", source.display()),
            format!("{name}:/src"),
        ];
        self.checked("source copy", copy, PULL_DEADLINE).await?;
        let copy = vec![
            "cp".into(),
            event.display().to_string(),
            format!("{name}:/bosn/event.json"),
        ];
        self.checked("event copy", copy, CONTROL_DEADLINE)
            .await
            .map(|_| ())
    }

    /// Make [`runner_tag`] present in the engine: `docker load` from the
    /// cache volume's image tar, or pull the pinned image once and save it.
    async fn load_runner(&self, name: &str) -> Result<(), String> {
        self.checked(
            "runner image",
            Self::exec(name, &load_runner_script()),
            PULL_DEADLINE,
        )
        .await
        .map(|_| ())
    }

    fn exec(name: &str, script: &str) -> Vec<String> {
        ["exec", name, "sh", "-ec", script]
            .into_iter()
            .map(String::from)
            .collect()
    }
}

fn owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

impl ActEngineBackend for DockerActBackend {
    fn resolve_engine_image(&self) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let inspect = owned(&["image", "inspect", "--format", "{{.Id}}", ENGINE_IMAGE]);
            if let Ok(id) = self
                .checked("engine image inspect", inspect.clone(), CONTROL_DEADLINE)
                .await
            {
                return Ok(id);
            }
            self.checked(
                "engine image pull",
                owned(&["pull", "-q", ENGINE_IMAGE]),
                PULL_DEADLINE,
            )
            .await?;
            self.checked("engine image inspect", inspect, CONTROL_DEADLINE)
                .await
        })
    }

    fn cache_bytes<'a>(&'a self, volume: &'a str) -> BoxFuture<'a, Result<Option<u64>, String>> {
        Box::pin(async move {
            if !self.volume_exists(volume).await? {
                return Ok(None);
            }
            // Measured read-only with the pinned engine image (present once
            // any run created the volume): no extra image is pulled.
            let mount = format!("type=volume,source={volume},target=/cache,readonly");
            let du = owned(&[
                "run",
                "--rm",
                "--mount",
                &mount,
                ENGINE_IMAGE,
                "du",
                "-sb",
                "/cache",
            ]);
            let out = self.checked("cache size", du, PULL_DEADLINE).await?;
            out.split_whitespace()
                .next()
                .and_then(|bytes| bytes.parse().ok())
                .map(Some)
                .ok_or_else(|| format!("cache size: unexpected output {out:?}"))
        })
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
            let exists = self
                .run(owned(&["volume", "inspect", &cache.name]), CONTROL_DEADLINE)
                .await?;
            if exists.ok() {
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

    fn create<'a>(&'a self, spec: &'a EngineSpec) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut args = owned(&["run", "-d", "--privileged", "--name"]);
            args.push(spec.name.clone());
            for (key, value) in &spec.labels {
                args.push("--label".into());
                args.push(format!("{key}={value}"));
            }
            // No host socket and no host path. The nested engine's storage is
            // the image's anonymous /var/lib/docker volume, removed with
            // `rm -v`; the named cache volume persists by design.
            args.push("--mount".into());
            args.push(format!(
                "type=volume,source={},target={ENGINE_CACHE}",
                spec.cache.name
            ));
            args.extend(owned(&[
                "--env",
                "DOCKER_TLS_CERTDIR=",
                ENGINE_IMAGE,
                "dockerd",
                "--host=unix:///var/run/docker.sock",
            ]));
            self.checked("engine create", args, PULL_DEADLINE)
                .await
                .map(|_| ())
        })
    }

    fn inspect<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ActEngineObservation>, String>> {
        Box::pin(async move {
            let result = self
                .run(
                    owned(&["container", "inspect", "--format", "{{json .}}", name]),
                    CONTROL_DEADLINE,
                )
                .await?;
            if !result.ok() {
                let stderr = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
                // Only Docker's own "no such container" proves absence.
                if stderr.contains("no such container") || stderr.contains("no such object") {
                    return Ok(None);
                }
                return Err(format!("engine inspect failed: {}", stderr.trim()));
            }
            let inspected: ContainerInspect = serde_json::from_slice(&result.stdout)
                .map_err(|_| "engine inspect returned an unexpected document".to_string())?;
            Ok(Some(ActEngineObservation {
                name: inspected.name.trim_start_matches('/').to_string(),
                engine_id: inspected.id,
                image_digest: inspected.image,
                labels: inspected.config.labels.unwrap_or_default(),
            }))
        })
    }

    fn prepare<'a>(
        &'a self,
        name: &'a str,
        source: &'a Path,
        event: &'a Path,
        act: ActArtifact,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.wait_ready(name).await?;
            self.install_act(name, act).await?;
            self.copy_inputs(name, source, event).await?;
            self.load_runner(name).await
        })
    }

    fn list<'a>(
        &'a self,
        name: &'a str,
        workflow: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.checked(
                "act -l",
                owned(&["exec", "-w", "/src", name, "act", "-l", "-W", workflow]),
                CONTROL_DEADLINE,
            )
            .await
        })
    }

    fn execute<'a>(
        &'a self,
        name: &'a str,
        invocation: &'a ActInvocation,
        deadline: Duration,
        cancellation: &'a CancellationToken,
        lines: &'a async_engine::Sender<EngineLine>,
    ) -> BoxFuture<'a, Result<ExecEnd, String>> {
        Box::pin(async move {
            // Secret values reach act only through the docker client's own
            // environment (`--env NAME` copies it); never through argv.
            let mut args = owned(&["exec", "-w", "/src"]);
            for (key, _) in &invocation.secrets.0 {
                args.push("--env".into());
                args.push(key.clone());
            }
            args.push(name.into());
            args.push("act".into());
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

    fn remove<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.checked(
                "engine removal",
                owned(&["rm", "-f", "-v", name]),
                CONTROL_DEADLINE,
            )
            .await
            .map(|_| ())
        })
    }
}

/// Shell (busybox) that installs act from the cache volume, refreshing a
/// missing or corrupt tarball from the pinned URL; prints `act --version`.
fn install_act_script(act: ActArtifact) -> String {
    format!(
        "tgz={ENGINE_CACHE}/tools/act-{ACT_VERSION}-{sum}.tgz; mkdir -p {ENGINE_CACHE}/tools; \
         if ! echo \"{sum}  $tgz\" | sha256sum -c - >/dev/null 2>&1; then \
           wget -q -O \"$tgz.$$\" '{url}' && \
           echo \"{sum}  $tgz.$$\" | sha256sum -c - >/dev/null && mv \"$tgz.$$\" \"$tgz\"; \
         fi; \
         tar -xzf \"$tgz\" -C /usr/local/bin act && act --version",
        url = act.url,
        sum = act.sha256,
    )
}

/// Shell that loads the runner image tar from the cache volume, or pulls
/// the pinned image, tags it [`runner_tag`] and saves the tar atomically.
fn load_runner_script() -> String {
    let tag = runner_tag();
    let file = tag.replace([':', '/'], "-");
    format!(
        "tar={ENGINE_CACHE}/images/{file}.tar; mkdir -p {ENGINE_CACHE}/images; \
         if ! {{ [ -f \"$tar\" ] && docker load -q -i \"$tar\" >/dev/null; }}; then \
           docker pull -q {RUNNER_IMAGE} >/dev/null && docker tag {RUNNER_IMAGE} {tag} && \
           docker save -o \"$tar.$$\" {tag} && mv \"$tar.$$\" \"$tar\"; \
         fi; \
         docker image inspect {tag} >/dev/null"
    )
}

/// The fields of `docker container inspect` the ownership checks need.
#[derive(serde::Deserialize)]
struct ContainerInspect {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Config")]
    config: ContainerConfig,
}

#[derive(serde::Deserialize)]
struct ContainerConfig {
    #[serde(rename = "Labels")]
    labels: Option<BTreeMap<String, String>>,
}

/// Splits a byte stream into bounded UTF-8 lines.
#[derive(Default)]
struct LineBuffer {
    pending: Vec<u8>,
}
const MAX_LINE: usize = 64 * 1024;
impl LineBuffer {
    fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }
    fn drain_lines(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            out.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        if self.pending.len() > MAX_LINE {
            let line: Vec<u8> = self.pending.drain(..MAX_LINE).collect();
            out.push(String::from_utf8_lossy(&line).into_owned());
        }
        out
    }
    fn finish(&mut self) -> Vec<String> {
        let mut out = self.drain_lines();
        if !self.pending.is_empty() {
            out.push(String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_never_names_a_host_socket_and_pins_runners() {
        let args = ActInvocation {
            event: "push".into(),
            workflow: ".github/workflows/ci.yml".into(),
            job: Some("lint".into()),
            cache_namespace: "0123456789abcdef".into(),
            secrets: SecretEnv(vec![("GITHUB_TOKEN".into(), "ghp_secretvalue".into())]),
        }
        .args();
        assert!(args.iter().all(|a| !a.contains("docker.sock")));
        assert!(args.contains(&format!("ubuntu-latest={}", runner_tag())));
        assert!(args.contains(&format!("{ENGINE_CACHE}/actcache/0123456789abcdef")));
        assert!(args.contains(&format!("{ENGINE_CACHE}/actions")));
        assert!(args.windows(2).any(|w| w == ["-s", "GITHUB_TOKEN"]));
        assert!(
            args.iter().all(|a| !a.contains("ghp_secretvalue")),
            "no value in argv"
        );
        assert!(args.ends_with(&["-j".to_string(), "lint".to_string()]));
        assert!(RUNNER_IMAGE.contains("@sha256:") && ENGINE_IMAGE.contains("@sha256:"));
        assert!(act_artifact("x86_64").is_some() && act_artifact("riscv64").is_none());
        assert_eq!(runner_tag(), "bosn/act-runner:c58e2b364da0");
        let secrets = SecretEnv(vec![("GITHUB_TOKEN".into(), "ghp_secretvalue".into())]);
        assert!(
            !format!("{secrets:?}").contains("ghp_"),
            "Debug shows names only"
        );
    }

    #[test]
    fn cache_scripts_verify_the_pinned_act_and_save_atomically() {
        let act = act_artifact("x86_64").unwrap();
        let install = install_act_script(act);
        assert!(install.contains(act.sha256) && install.contains(act.url));
        assert!(install.contains("sha256sum -c"));
        let load = load_runner_script();
        assert!(load.contains("docker load") && load.contains(RUNNER_IMAGE));
        assert!(load.contains("mv \"$tar.$$\" \"$tar\""), "atomic rename");
        let cache = CacheVolume::machine("11111111-2222-4333-8444-555555555555", 1.0).unwrap();
        assert_eq!(cache.name, CACHE_VOLUME);
        assert!(
            bosn_core::REQUIRED_LABELS
                .iter()
                .all(|k| cache.labels.contains_key(*k))
        );
    }

    #[test]
    fn line_buffer_splits_and_bounds_lines() {
        let mut b = LineBuffer::default();
        b.push(b"one\ntw");
        assert_eq!(b.drain_lines(), ["one"]);
        b.push(b"o\n");
        assert_eq!(b.drain_lines(), ["two"]);
        b.push(&vec![b'x'; MAX_LINE + 5]);
        assert_eq!(b.drain_lines()[0].len(), MAX_LINE);
        assert_eq!(b.finish(), ["xxxxx"]);
    }
}
