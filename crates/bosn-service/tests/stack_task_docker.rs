//! Opt-in, live-Docker proofs for `bosn run --task` against a workspace
//! `bosn.toml` stack: the persistent setup container and `docker exec`.
//!
//! Each test drives the production `bosn` binary against its own temporary
//! workspace and state directory, then removes exactly the containers that
//! bind that workspace and the volumes they mounted. They need a local Docker
//! daemon and the pre-pulled `PINNED_ALPINE` image.

use crate::support::setup_docker::*;

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

    fn bosn_run(&self, task: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bosn"));
        command
            .current_dir(&self.workspace)
            .args(["run", "--task", task, "--state-dir"])
            .arg(&self.state);
        command
    }

    /// `bosn run --task NAME`; returns its stdout then its stderr (Bosn's
    /// own job lines), failing on a non-zero exit.
    fn run_task(&self, task: &str) -> String {
        let output = self.bosn_run(task).output().expect("run bosn run");
        Self::output_of(task, output)
    }

    fn output_of(task: &str, output: std::process::Output) -> String {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "bosn run --task {task} failed: {text}"
        );
        text
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

    fn running(&self, container: &str) -> bool {
        let result = docker_capture(
            &self.engine,
            [
                "container",
                "inspect",
                "--format",
                "{{.State.Running}}",
                container,
            ],
        );
        String::from_utf8_lossy(&result.stdout).trim() == "true"
    }

    fn top(&self, container: &str) -> String {
        let result = docker_capture(&self.engine, ["container", "top", container]);
        String::from_utf8_lossy(&result.stdout).into_owned()
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

/// One `live` stack built from `live.Dockerfile`, binding the workspace and
/// keeping a stack-scoped volume at `/state`, plus `tasks` as (name, cmd).
fn stack_manifest(marker: &str, tasks: &[(&str, &str)]) -> String {
    let mut manifest = format!(
        "[stack.live]\n\
         dockerfile = 'live.Dockerfile'\n\
         [stack.live.mounts]\n\
         ws = {{ source = '.', destination = '/ws', readonly = true }}\n\
         [stack.live.volumes]\n\
         state = {{ scope = 'stack', destination = '/state' }}\n\
         [stack.live.env]\n\
         BOSN_LIVE_MARKER = '{marker}'\n"
    );
    for (name, cmd) in tasks {
        manifest.push_str(&format!(
            "[task.{name}]\nstack = 'live'\ncmd = '''{cmd}'''\n"
        ));
    }
    manifest
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test integration --locked -- --ignored --exact stack_task_docker::live_docker_stack_task_runs_with_the_conventional_umask`
///
/// `docker exec` starts its process with umask 0000, so without an explicit
/// umask every file a task created was group- and world-writable (#365).
/// Tasks run with 0022, as on a host login and on GitHub runners.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_stack_task_runs_with_the_conventional_umask() {
    let stack = StackWorkspace::new(&stack_manifest(
        &test_unique_suffix(),
        &[(
            "probe",
            "printf 'umask=%s\\n' \"$(umask)\"; mkdir /tmp/bosn-umask-dir; \
         touch /tmp/bosn-umask-file; \
         stat -c 'mode=%a' /tmp/bosn-umask-dir /tmp/bosn-umask-file",
        )],
    ));
    let stdout = stack.run_task("probe");
    assert!(stdout.contains("umask=0022"), "task umask: {stdout}");
    assert!(
        stdout.contains("mode=755\nmode=644\n"),
        "created modes: {stdout}"
    );
}

/// A task-started daemon that bumps a counter in the stack-scoped `/state`
/// volume every 200 ms, detached so it outlives its `docker exec`.
const HEARTBEAT_DAEMON: &str = "setsid sh -c 'i=0; while :; do i=$((i+1)); \
     echo $i > /state/beat; sleep 0.2; done' </dev/null >/dev/null 2>&1 & \
     sleep 0.5; echo daemon=started";

/// Reports whether anything still writes `/state/beat`.
const HEARTBEAT_PROBE: &str = "a=$(cat /state/beat); sleep 1.5; b=$(cat /state/beat); \
     if [ \"$a\" = \"$b\" ]; then echo heartbeat=still; else echo heartbeat=alive; fi";

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test integration --locked -- --ignored --exact stack_task_docker::live_docker_a_retired_generation_stops_before_the_next_one_runs`
///
/// A stack's generation rolls over when its `bosn.toml` changes. The retired
/// container used to keep running, with any daemon a task started in it, in
/// the stack-scoped volumes the new generation mounts too: soldr-broker's
/// socket in `/root/.soldr` broke every later build (#383). The retired
/// container is stopped before the new generation's task runs.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_a_retired_generation_stops_before_the_next_one_runs() {
    let tasks = [("daemon", HEARTBEAT_DAEMON), ("probe", HEARTBEAT_PROBE)];
    let unique = test_unique_suffix();
    let mut stack = StackWorkspace::new(&stack_manifest(&format!("{unique}-one"), &tasks));
    assert!(stack.run_task("daemon").contains("daemon=started"));
    assert!(
        stack.run_task("probe").contains("heartbeat=alive"),
        "the probe must see a live daemon in its own generation"
    );
    let retired = stack.containers();
    assert_eq!(
        retired.len(),
        1,
        "one first-generation container: {retired:?}"
    );

    stack.write_manifest(&stack_manifest(&format!("{unique}-two"), &tasks));
    let stdout = stack.run_task("probe");
    assert!(
        stdout.contains("heartbeat=still"),
        "the retired generation's daemon still writes the shared volume: {stdout}"
    );
    assert!(
        stdout.contains(&format!("stopped retired generation {}", retired[0])),
        "the task log names the container it stopped: {stdout}"
    );
    let running = stack.running(&retired[0]);
    assert!(
        !running,
        "retired container {} is still running",
        retired[0]
    );
    assert_eq!(
        stack.containers().len(),
        2,
        "the retired container is kept for GC"
    );
}

/// Run with:
/// `soldr cargo test -j1 -p bosn-service --test integration --locked -- --ignored --exact stack_task_docker::live_docker_a_retired_generation_keeps_running_until_its_last_task_ends`
///
/// A task still running in a retired generation holds an execution session,
/// which protects its container (#383): the next generation's task does not
/// kill it, and the container stops when that last task ends.
#[test]
#[ignore = "requires a local Docker daemon and the pinned Alpine image"]
fn live_docker_a_retired_generation_keeps_running_until_its_last_task_ends() {
    let tasks = [
        ("hold", "sleep 7; echo held=done"),
        ("probe", "echo probed"),
    ];
    let unique = test_unique_suffix();
    let mut stack = StackWorkspace::new(&stack_manifest(&format!("{unique}-one"), &tasks));
    stack.run_task("probe");
    let retired = stack.containers();
    assert_eq!(
        retired.len(),
        1,
        "one first-generation container: {retired:?}"
    );
    let hold = stack
        .bosn_run("hold")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the long first-generation task");
    let deadline = Instant::now() + JOB_DEADLINE;
    while !stack.top(&retired[0]).contains("sleep 7") {
        assert!(Instant::now() < deadline, "the hold task never started");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The rollover happens while `hold` runs. The daemon may queue `probe`
    // behind it; either way `hold` must finish on its own and stop its
    // container itself.
    stack.write_manifest(&stack_manifest(&format!("{unique}-two"), &tasks));
    let probed = stack.run_task("probe");
    let held = StackWorkspace::output_of("hold", hold.wait_with_output().expect("hold task"));
    assert!(
        held.contains("held=done"),
        "the hold task was cut short: {held}"
    );
    assert!(
        held.contains(&format!("stopped retired generation {}", retired[0])),
        "the last task in the retired container stops it: {held}"
    );
    assert!(
        !probed.contains("stopped retired generation"),
        "a container with a running task was stopped under it: {probed}"
    );
    assert!(
        !stack.running(&retired[0]),
        "retired container still running"
    );
}
