//! Shared live-Docker helpers for the setup ensure/adopt/GC proofs.

pub(crate) use std::{
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) use super::tls_setup_server::{TlsSetupServer, certificate_path};
pub(crate) use bosn_core::{ResourceKind, ResourceState};
pub(crate) use bosn_engine::{CommandResult, DockerEngine, RunOptions};
pub(crate) use bosn_registry::Registry;
pub(crate) use bosn_service::{
    Client, SetupAdoptRequest, SetupEnsureJobRequest, SetupPreparePolicy,
};
pub(crate) use bosn_setup::{SetupAcquirePolicy, SetupPlanRequest, plan_setup};
pub(crate) use kernal_api::{async_engine::RuntimeBuilder, hash::sha256_bytes};

pub(crate) const PINNED_ALPINE: &str =
    "alpine@sha256:28bd5fe8b56d1bd048e5babf5b10710ebe0bae67db86916198a6eec434943f8b";
pub(crate) const READY_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) const JOB_DEADLINE: Duration = Duration::from_secs(90);
pub(crate) const DOCKER_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) const OUTPUT_LIMIT: usize = 1024 * 1024;
pub(crate) const MANAGED_LABEL: &str = "com.zackees.bosn.setup-managed";
pub(crate) const CONTENT_LABEL: &str = "com.zackees.bosn.setup-content-sha256";
pub(crate) const NAME_LABEL: &str = "com.zackees.bosn.setup-container";

/// A child daemon is reaped even if an assertion fails before the normal
/// authenticated shutdown path runs.
pub(crate) struct DaemonChild {
    pub(crate) child: Child,
}

impl DaemonChild {
    pub(crate) fn start(state: &Path) -> Self {
        Self::start_with_certificate(state, None)
    }

    pub(crate) fn start_with_certificate(state: &Path, certificate: Option<&Path>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command
            .args(["daemon", "serve", "--state-dir"])
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(certificate) = certificate {
            command
                .env("SSL_CERT_FILE", certificate)
                .env("NO_PROXY", "localhost,127.0.0.1");
        }
        Self {
            child: command.spawn().expect("start production Bosn daemon"),
        }
    }

    pub(crate) fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + READY_DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().expect("observe daemon") {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for DaemonChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ContainerInspection {
    pub(crate) id: String,
    pub(crate) running: bool,
    pub(crate) image: String,
    pub(crate) managed: String,
    pub(crate) content_sha256: String,
    pub(crate) container_name: String,
}

pub(crate) fn docker_capture(
    engine: &DockerEngine,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
) -> CommandResult {
    engine
        .with_args(args)
        .capture(RunOptions::bounded(DOCKER_DEADLINE, 64 * 1024))
        .expect("run bounded kernal-api Docker command")
}

pub(crate) fn inspect_container(
    engine: &DockerEngine,
    container_name: &str,
) -> Result<Option<ContainerInspection>, String> {
    let inspect_format = format!(
        "{{{{.Id}}}}\t{{{{.State.Running}}}}\t{{{{.Config.Image}}}}\t{{{{index .Config.Labels \"{MANAGED_LABEL}\"}}}}\t{{{{index .Config.Labels \"{CONTENT_LABEL}\"}}}}\t{{{{index .Config.Labels \"{NAME_LABEL}\"}}}}"
    );
    let result = docker_capture(
        engine,
        [
            "container",
            "inspect",
            "--format",
            inspect_format.as_str(),
            container_name,
        ],
    );
    if result.exit_code == 1 {
        return Ok(None);
    }
    if !result.ok() {
        return Err(format!(
            "docker inspect exited {}: {}",
            result.exit_code,
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    let text = String::from_utf8(result.stdout)
        .map_err(|_| "docker inspect returned non-UTF-8 output".to_owned())?;
    let fields: Vec<_> = text.trim_end_matches(['\r', '\n']).split('\t').collect();
    if fields.len() != 6 || fields.iter().any(|field| field.is_empty()) {
        return Err("docker inspect returned an incomplete Bosn container record".into());
    }
    let running = match fields[1] {
        "true" => true,
        "false" => false,
        _ => return Err("docker inspect returned an invalid running value".into()),
    };
    Ok(Some(ContainerInspection {
        id: fields[0].into(),
        running,
        image: fields[2].into(),
        managed: fields[3].into(),
        content_sha256: fields[4].into(),
        container_name: fields[5].into(),
    }))
}

/// Removes only a container this test can still prove is its own.  It never
/// uses a label selector, prune, or a name supplied by Docker output.
pub(crate) struct ExactContainerCleanup {
    pub(crate) engine: DockerEngine,
    pub(crate) container_name: String,
    pub(crate) content_sha256: String,
}

impl Drop for ExactContainerCleanup {
    fn drop(&mut self) {
        if self.container_name.is_empty() {
            match setup_container_for(&self.engine, &self.content_sha256) {
                Some(name) => self.container_name = name,
                None => return,
            }
        }
        match inspect_container(&self.engine, &self.container_name) {
            Ok(None) => {}
            Ok(Some(observed))
                if observed.managed == "v1"
                    && observed.content_sha256 == self.content_sha256
                    && observed.container_name == self.container_name =>
            {
                let result = docker_capture(
                    &self.engine,
                    ["container", "rm", "--force", self.container_name.as_str()],
                );
                if !result.ok() {
                    eprintln!(
                        "live setup ensure cleanup could not remove {}: {}",
                        self.container_name,
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
            }
            Ok(Some(_)) => eprintln!(
                "live setup ensure cleanup refused unexpected container {}",
                self.container_name
            ),
            Err(error) => eprintln!(
                "live setup ensure cleanup could not inspect {}: {error}",
                self.container_name
            ),
        }
    }
}

/// The one managed setup container this test's unique document created,
/// found by its content label. Since #349 the container name is the
/// creation identity (canonical workspace, creation arguments), which a
/// test cannot know before its image is prepared.
pub(crate) fn setup_container_for(engine: &DockerEngine, content_sha256: &str) -> Option<String> {
    let filter = format!("label=com.zackees.bosn.setup-content-sha256={content_sha256}");
    let result = docker_capture(
        engine,
        [
            "container",
            "ls",
            "--all",
            "--filter",
            filter.as_str(),
            "--format",
            "{{.Names}}",
        ],
    );
    assert!(result.ok(), "docker container ls failed");
    let text = String::from_utf8(result.stdout).expect("container names");
    let names: Vec<&str> = text.split_whitespace().collect();
    assert!(
        names.len() <= 1,
        "more than one container carries {content_sha256}: {names:?}"
    );
    names.first().map(|name| (*name).to_owned())
}

pub(crate) fn wait_for_client(
    runtime: &kernal_api::async_engine::Runtime,
    daemon: &mut DaemonChild,
    state: &Path,
) -> Client {
    let client = Client::for_state(state).expect("construct daemon client");
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if runtime.run(client.ping()).is_ok() {
            return client;
        }
        assert!(
            daemon.child.try_wait().expect("observe daemon").is_none(),
            "daemon exited before becoming ready"
        );
        assert!(Instant::now() < deadline, "daemon did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn wait_for_success(
    runtime: &kernal_api::async_engine::Runtime,
    client: &Client,
    job_id: u64,
) {
    let deadline = Instant::now() + JOB_DEADLINE;
    loop {
        let status = runtime
            .run(client.job_status(job_id))
            .expect("read setup ensure job status");
        match status.state.as_str() {
            "Succeeded" => return,
            "Failed" | "Cancelled" | "Superseded" => {
                let logs = runtime
                    .run(client.job_logs(job_id, 0, 64))
                    .map(|page| {
                        page.records
                            .into_iter()
                            .map(|record| record.line)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                panic!(
                    "setup ensure job {job_id} ended {}: {:?}; logs: {logs:?}",
                    status.state, status.error,
                )
            }
            "Queued" | "Running" | "Cancelling" => {}
            unexpected => panic!("unexpected setup ensure job state {unexpected}"),
        }
        assert!(
            Instant::now() < deadline,
            "setup ensure job {job_id} timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn image_identity_for(engine: &DockerEngine, reference: &str) -> String {
    let result = docker_capture(
        engine,
        ["image", "inspect", "--format", "{{.Id}}", reference],
    );
    assert!(
        result.ok(),
        "Docker image {reference:?} is unavailable: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let identity = String::from_utf8(result.stdout).expect("image identity is UTF-8");
    let identity = identity.trim().to_owned();
    assert!(!identity.is_empty(), "pinned Alpine image has no identity");
    identity
}

pub(crate) fn pinned_alpine_identity(engine: &DockerEngine) -> String {
    image_identity_for(engine, PINNED_ALPINE)
}

pub(crate) fn test_unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}

/// A test's own registry, returning its id. On a machine that already has Bosn objects, a daemon
/// refuses to mint an identity in an empty state directory (#515), and a pass must judge every
/// pre-existing object as foreign to it.
pub(crate) fn own_registry(state: &Path) -> String {
    std::fs::create_dir_all(state).expect("create state directory");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let registry_id = format!(
        "{:08x}-0000-4000-8000-{:012x}",
        (nanos >> 48) as u32,
        nanos & 0xffff_ffff_ffff
    );
    drop(Registry::create_writer(state.join("registry.sqlite3"), &registry_id).expect("registry"));
    registry_id
}
