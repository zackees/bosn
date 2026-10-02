//! Opt-in, live-Docker proofs for `bosn run --task` against a workspace
//! `bosn.toml` stack: the persistent setup container and `docker exec`.
//!
//! Each test drives the production `bosn` binary against its own temporary
//! workspace and state directory, then removes exactly the containers that
//! bind that workspace and the volumes they mounted. They need a local Docker
//! daemon and the pre-pulled `PINNED_ALPINE` image.

mod support;

use support::setup_docker::*;

/// A temporary workspace, its private daemon, and teardown of every
/// container (and its volumes) that binds the workspace.
struct StackWorkspace {
    engine: DockerEngine,
    _root: tempfile::TempDir,
    workspace: std::path::PathBuf,
    state: std::path::PathBuf,
    daemon: Option<DaemonChild>,
}

impl StackWorkspace {
    fn new(manifest: &str) -> Self {
        let engine = DockerEngine::docker();
        pinned_alpine_identity(&engine);
        let root = tempfile::tempdir().expect("temporary test root");
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        std::fs::create_dir(&workspace).expect("create workspace");
        let workspace = workspace.canonicalize().expect("canonical workspace");
        let mut this = Self {
            engine,
            _root: root,
            workspace,
            state,
            daemon: None,
        };
        this.write_manifest(manifest);
        // Alpine's /etc/profile sets `umask 022`; Debian's and Ubuntu's do not,
        // so the image drops that line to show the umask a task gets from Bosn.
        std::fs::write(
            this.workspace.join("live.Dockerfile"),
            format!("FROM {PINNED_ALPINE}\nRUN sed -i '/^umask/d' /etc/profile\n"),
        )
        .expect("write live.Dockerfile");
        let mut daemon = DaemonChild::start(&this.state);
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("construct kernal-api runtime");
        wait_for_client(&runtime, &mut daemon, &this.state);
        this.daemon = Some(daemon);
        this
    }

    fn write_manifest(&mut self, manifest: &str) {
        std::fs::write(self.workspace.join("bosn.toml"), manifest).expect("write bosn.toml");
    }

    /// `bosn run --task NAME`; returns its stdout, failing on a non-zero exit.
    fn run_task(&self, task: &str) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_bosn"))
            .current_dir(&self.workspace)
            .args(["run", "--task", task, "--state-dir"])
            .arg(&self.state)
            .output()
            .expect("run bosn run");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "bosn run --task {task} failed: {stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    /// Every container, running or not, that binds this test's workspace.
    fn containers(&self) -> Vec<String> {
        let listed = docker_capture(
            &self.engine,
            [
                "container",
                "ls",
                "--all",
                "--filter",
                "label=com.zackees.bosn.setup-managed",
                "--format",
                "{{.Names}}",
            ],
        );
        assert!(listed.ok(), "docker container ls failed");
        String::from_utf8_lossy(&listed.stdout)
            .split_whitespace()
            .filter(|name| {
                self.mount_field(name, "Source")
                    .iter()
                    .any(|source| std::path::Path::new(source) == self.workspace)
            })
            .map(str::to_owned)
            .collect()
    }

    fn mount_field(&self, container: &str, field: &str) -> Vec<String> {
        let format = format!("{{{{range .Mounts}}}}{{{{.{field}}}}}\n{{{{end}}}}");
        let result = docker_capture(
            &self.engine,
            [
                "container",
                "inspect",
                "--format",
                format.as_str(),
                container,
            ],
        );
        String::from_utf8_lossy(&result.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for StackWorkspace {
    fn drop(&mut self) {
        drop(self.daemon.take());
        let mut images = Vec::new();
        for container in self.containers() {
            let volumes = self.mount_field(&container, "Name");
            let image = docker_capture(
                &self.engine,
                [
                    "container",
                    "inspect",
                    "--format",
                    "{{.Config.Image}}",
                    &container,
                ],
            );
            images.push(String::from_utf8_lossy(&image.stdout).trim().to_owned());
            docker_capture(&self.engine, ["container", "rm", "--force", &container]);
            for volume in volumes.iter().filter(|name| name.starts_with("bosn-v-")) {
                docker_capture(&self.engine, ["volume", "rm", volume.as_str()]);
            }
        }
        // The image Bosn built from this workspace's `live.Dockerfile`.
        for image in images
            .iter()
            .filter(|image| image.starts_with("bosn-setup:"))
        {
            docker_capture(&self.engine, ["image", "rm", image.as_str()]);
        }
    }
}

fn stack_manifest(marker: &str, task_cmd: &str) -> String {
    format!(
        "[stack.live]\n\
         dockerfile = 'live.Dockerfile'\n\
         [stack.live.mounts]\n\
         ws = {{ source = '.', destination = '/ws', readonly = true }}\n\
         [stack.live.env]\n\
         BOSN_LIVE_MARKER = '{marker}'\n\
         [task.probe]\n\
         stack = 'live'\n\
         cmd = '''{task_cmd}'''\n"
    )
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test stack_task_docker --locked -- --ignored --exact live_docker_stack_task_runs_with_the_conventional_umask`
///
/// `docker exec` starts its process with umask 0000, so without an explicit
/// umask every file a task created was group- and world-writable (#365).
/// Tasks run with 0022, as on a host login and on GitHub runners.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_stack_task_runs_with_the_conventional_umask() {
    let stack = StackWorkspace::new(&stack_manifest(
        &test_unique_suffix(),
        "printf 'umask=%s\\n' \"$(umask)\"; mkdir /tmp/bosn-umask-dir; \
         touch /tmp/bosn-umask-file; \
         stat -c 'mode=%a' /tmp/bosn-umask-dir /tmp/bosn-umask-file",
    ));
    let stdout = stack.run_task("probe");
    assert!(stdout.contains("umask=0022"), "task umask: {stdout}");
    assert!(
        stdout.contains("mode=755\nmode=644\n"),
        "created modes: {stdout}"
    );
}
