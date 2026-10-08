//! `bosn run`: the ergonomic front door for a workspace `bosn.toml`.
//!
//! It composes existing daemon-owned operations and adds no new authority:
//! `bosn run --task NAME` is exactly `manifest ensure` of the task's stack
//! followed by `manifest app-task`, with the workspace taken from the
//! manifest's directory, the daemon started on demand, and the job logs
//! streamed until the job ends. The process exits with the task's exit code.

use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use bosn_service::{
    Client, JobStatus, MANIFEST_MAX_DEADLINE, MANIFEST_MAX_OUTPUT, ManifestAppTaskJobRequest,
    ManifestEnsureJobRequest,
};
use kernal_api::async_engine::{self, Runtime, RuntimeBuilder};

/// Default budget for the ensure step (a cold Dockerfile build).
pub const DEFAULT_ENSURE_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// Default budget for the declared task (a full CI job under `act`).
pub const DEFAULT_TASK_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);
const DAEMON_START_WAIT: Duration = Duration::from_secs(20);
/// The task job is cancelled once this client has not polled it for this
/// long (#357): a `bosn run` killed by SIGTERM, SIGHUP or SIGKILL no longer
/// leaves its job running to the deadline. Polls happen every
/// [`POLL_INTERVAL`], so a live client never comes close.
pub const FOLLOW_LEASE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Exit status for a Bosn-side failure (as opposed to the task's own status).
const EXIT_BOSN_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_INTERRUPTED: i32 = 130;

#[derive(Debug, PartialEq, Eq)]
pub struct RunArguments {
    pub task: Option<String>,
    pub stack: Option<String>,
    /// `None` means the nearest `bosn.toml` in the current directory or an
    /// ancestor, as the Python CLI resolved it.
    pub manifest: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub deadline: Option<Duration>,
    pub output_limit: usize,
    pub no_ensure: bool,
}

pub fn run(arguments: impl Iterator<Item = OsString>) {
    let arguments = parse_arguments(arguments).unwrap_or_else(|message| {
        eprintln!("bosn run: {message}");
        eprintln!("{USAGE}");
        std::process::exit(EXIT_USAGE)
    });
    let code = execute(arguments).unwrap_or_else(|message| {
        eprintln!("bosn run: {message}");
        EXIT_BOSN_FAILURE
    });
    std::process::exit(code)
}

pub const USAGE: &str = "usage: bosn run --task NAME [--stack NAME] [--manifest PATH] [--state-dir STATE_DIR] [--deadline-ms 1..=14400000] [--output-limit 1..=67108864] [--no-ensure]\n   or: bosn run --stack NAME [--manifest PATH] [--state-dir STATE_DIR] [--deadline-ms 1..=14400000] [--output-limit 1..=67108864]";

pub fn parse_arguments(
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<RunArguments, String> {
    let mut task = None;
    let mut stack = None;
    let mut manifest = None;
    let mut state_dir = None;
    let mut deadline = None;
    let mut output_limit = None;
    let mut no_ensure = false;
    while let Some(flag) = arguments.next() {
        let flag = flag
            .into_string()
            .map_err(|_| "arguments must be UTF-8".to_owned())?;
        if flag == "--no-ensure" {
            if no_ensure {
                return Err("--no-ensure given twice".into());
            }
            no_ensure = true;
            continue;
        }
        let value = arguments
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let text = || {
            value
                .clone()
                .into_string()
                .map_err(|_| format!("{flag} value must be UTF-8"))
        };
        let duplicate = || format!("{flag} given twice");
        match flag.as_str() {
            "--task" => {
                let name = valid_name(&text()?).ok_or("--task is not a valid task name")?;
                task.replace(name).map_or(Ok(()), |_| Err(duplicate()))?;
            }
            "--stack" => {
                let name = valid_name(&text()?).ok_or("--stack is not a valid stack name")?;
                stack.replace(name).map_or(Ok(()), |_| Err(duplicate()))?;
            }
            "--manifest" => {
                if value.is_empty() {
                    return Err("--manifest must not be empty".into());
                }
                manifest
                    .replace(PathBuf::from(&value))
                    .map_or(Ok(()), |_| Err(duplicate()))?;
            }
            "--state-dir" => {
                if value.is_empty() {
                    return Err("--state-dir must not be empty".into());
                }
                state_dir
                    .replace(PathBuf::from(&value))
                    .map_or(Ok(()), |_| Err(duplicate()))?;
            }
            "--deadline-ms" => {
                let milliseconds: u64 = text()?
                    .parse()
                    .map_err(|_| "--deadline-ms must be an integer".to_owned())?;
                let value = Duration::from_millis(milliseconds);
                if value.is_zero() || value > MANIFEST_MAX_DEADLINE {
                    return Err(format!(
                        "--deadline-ms must be 1..={}",
                        MANIFEST_MAX_DEADLINE.as_millis()
                    ));
                }
                deadline
                    .replace(value)
                    .map_or(Ok(()), |_| Err(duplicate()))?;
            }
            "--output-limit" => {
                let value: usize = text()?
                    .parse()
                    .map_err(|_| "--output-limit must be an integer".to_owned())?;
                if value == 0 || value > MANIFEST_MAX_OUTPUT {
                    return Err(format!("--output-limit must be 1..={MANIFEST_MAX_OUTPUT}"));
                }
                output_limit
                    .replace(value)
                    .map_or(Ok(()), |_| Err(duplicate()))?;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    if task.is_none() && stack.is_none() {
        return Err("--task or --stack is required".into());
    }
    if task.is_none() && no_ensure {
        return Err("--no-ensure requires --task".into());
    }
    Ok(RunArguments {
        task,
        stack,
        manifest,
        state_dir,
        deadline,
        output_limit: output_limit.unwrap_or(MANIFEST_MAX_OUTPUT),
        no_ensure,
    })
}

fn valid_name(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'))
    .then(|| value.to_owned())
}

/// The nearest `bosn.toml` in `start` or one of its ancestors.
pub fn find_manifest(start: &Path) -> Result<PathBuf, String> {
    start
        .ancestors()
        .map(|directory| directory.join("bosn.toml"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            format!(
                "no bosn.toml in {} or any parent directory; pass --manifest",
                start.display()
            )
        })
}

/// Split a manifest path into the daemon's (workspace, relative manifest)
/// request shape: the workspace is the manifest's own directory.
pub fn manifest_location(manifest: &Path) -> Result<(PathBuf, String), String> {
    let canonical = std::fs::canonicalize(manifest)
        .map_err(|_| format!("manifest {} does not exist", manifest.display()))?;
    if !canonical.is_file() {
        return Err(format!("manifest {} is not a file", manifest.display()));
    }
    let workspace = canonical
        .parent()
        .ok_or_else(|| "manifest has no parent directory".to_owned())?
        .to_path_buf();
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "manifest file name is not UTF-8".to_owned())?
        .to_owned();
    Ok((workspace, name))
}

/// Resolve which stack to ensure and run in. Local parsing here is inert: the
/// daemon re-reads and re-validates the manifest before any engine work.
pub fn resolve_stack(
    manifest_text: &str,
    workspace: &Path,
    task: Option<&str>,
    stack: Option<&str>,
) -> Result<String, String> {
    let manifest = bosn_core::manifest::parse_manifest_toml(
        manifest_text,
        bosn_core::manifest::ManifestRoots::new(
            "bosn.toml",
            workspace.to_string_lossy(),
            workspace.to_string_lossy(),
        ),
    )
    .map_err(|error| format!("manifest is invalid: {error}"))?;
    match (task, stack) {
        (Some(task), stack) => {
            let declared = manifest.task(task).map_err(|error| error.to_string())?;
            match stack {
                Some(stack) if stack != declared.stack => Err(format!(
                    "task {task} belongs to stack {}, not {stack}",
                    declared.stack
                )),
                _ => Ok(declared.stack.clone()),
            }
        }
        (None, Some(stack)) => manifest
            .stack(stack)
            .map(|stack| stack.name.clone())
            .map_err(|error| error.to_string()),
        (None, None) => Err("--task or --stack is required".into()),
    }
}

/// The task's own exit status, recovered from the daemon's stable failure
/// text (`SetupTaskError::TaskFailed` / the guest SSH equivalent).
pub fn task_exit_code(error: &str) -> Option<i32> {
    const MARKERS: [&str; 2] = [
        "declared setup task exited with ",
        "declared manifest guest task exited with ",
    ];
    MARKERS.iter().find_map(|marker| {
        let rest = &error[error.find(marker)? + marker.len()..];
        let digits: String = rest
            .chars()
            .take_while(|character| character.is_ascii_digit() || *character == '-')
            .collect();
        let code: i32 = digits.parse().ok()?;
        Some(if (1..=255).contains(&code) { code } else { 1 })
    })
}

fn execute(arguments: RunArguments) -> Result<i32, String> {
    let manifest_path = match &arguments.manifest {
        Some(path) => path.clone(),
        None => find_manifest(
            &std::env::current_dir()
                .map_err(|_| "the current directory is unavailable".to_owned())?,
        )?,
    };
    let (workspace, manifest) = manifest_location(&manifest_path)?;
    let manifest_text = std::fs::read_to_string(workspace.join(&manifest))
        .map_err(|_| "manifest is not a readable UTF-8 file".to_owned())?;
    let stack = resolve_stack(
        &manifest_text,
        &workspace,
        arguments.task.as_deref(),
        arguments.stack.as_deref(),
    )?;
    let state_dir = arguments
        .state_dir
        .clone()
        .unwrap_or_else(bosn_service::mcp::default_state_dir);
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .map_err(|_| "cannot start the async runtime".to_owned())?;
    let client = Client::for_state(&state_dir).map_err(|_| "invalid state directory")?;
    ensure_daemon(&runtime, &client, &state_dir)?;

    if !arguments.no_ensure {
        eprintln!(
            "bosn run: ensuring stack {stack} for {}",
            workspace.display()
        );
        let job = runtime
            .run(client.submit_manifest_ensure(ManifestEnsureJobRequest {
                workspace: workspace.clone(),
                manifest: manifest.clone(),
                stack: stack.clone(),
                deadline: arguments.deadline.unwrap_or(DEFAULT_ENSURE_DEADLINE),
                output_limit: arguments.output_limit,
            }))
            .map_err(|error| format!("manifest ensure was refused: {error}"))?;
        let status = follow_job(&runtime, &client, &state_dir, job, false)?;
        match status.state.as_str() {
            "Succeeded" => {}
            "Cancelled" => return Ok(EXIT_INTERRUPTED),
            state => {
                return Err(format!(
                    "ensure of stack {stack} {} (job {job}): {}",
                    state.to_lowercase(),
                    status.error.unwrap_or_default()
                ));
            }
        }
    }
    let Some(task) = arguments.task else {
        eprintln!("bosn run: stack {stack} is ensured");
        return Ok(0);
    };
    eprintln!("bosn run: running task {task} in stack {stack}");
    let job = runtime
        .run(client.follow_manifest_app_task(
            ManifestAppTaskJobRequest {
                workspace,
                manifest,
                stack,
                task_name: task.clone(),
                deadline: arguments.deadline.unwrap_or(DEFAULT_TASK_DEADLINE),
                output_limit: arguments.output_limit,
            },
            FOLLOW_LEASE,
        ))
        .map_err(|error| format!("manifest app task was refused: {error}"))?;
    eprintln!("{}", job_banner(job, &state_dir));
    let status = follow_job(&runtime, &client, &state_dir, job, true)?;
    match status.state.as_str() {
        "Succeeded" => Ok(0),
        "Cancelled" => Ok(EXIT_INTERRUPTED),
        _ => {
            let error = status.error.unwrap_or_default();
            match task_exit_code(&error) {
                Some(code) => {
                    eprintln!("bosn run: task {task} exited with {code}");
                    Ok(code)
                }
                None => Err(format!("task {task} did not complete (job {job}): {error}")),
            }
        }
    }
}

/// Start `bosn daemon serve` for this state directory when none answers.
pub(crate) fn ensure_daemon(
    runtime: &Runtime,
    client: &Client,
    state_dir: &Path,
) -> Result<(), String> {
    register_maintenance();
    if let Ok(version) = runtime.run(client.daemon_version()) {
        return matching_daemon(state_dir, &version);
    }
    let executable = std::env::current_exe()
        .map_err(|_| "cannot locate the bosn executable to start its daemon".to_owned())?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("daemon")
        .arg("serve")
        .arg("--state-dir")
        .arg(state_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the bosn daemon: {error}"))?;
    let started = Instant::now();
    while started.elapsed() < DAEMON_START_WAIT {
        if let Ok(version) = runtime.run(client.daemon_version()) {
            // Another session may have won the start race with its own binary.
            matching_daemon(state_dir, &version)?;
            eprintln!("bosn: started the bosn daemon for {}", state_dir.display());
            return Ok(());
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(daemon_start_failure(state_dir))
}

fn register_maintenance() {
    // Isolated tests and CI must not register host login services.
    if std::env::var_os("BOSN_TEST_ISOLATED").is_some() || std::env::var_os("CI").is_some() {
        return;
    }
    let Some(platform) = bosn_service::autostart::Platform::current() else {
        eprintln!(
            "bosn: persistent maintenance registration is unsupported on this platform; automatic retention requires a running daemon"
        );
        return;
    };
    let Some(home) = kernal_api::platform::host::home_dir() else {
        eprintln!("bosn: cannot register persistent maintenance: home directory unavailable");
        return;
    };
    let result = std::env::current_exe()
        .map_err(|error| error.to_string())
        .and_then(|binary| {
            bosn_service::autostart::ensure_default(
                &bosn_service::autostart::SystemRunner,
                platform,
                &home,
                &binary,
                &bosn_service::mcp::machine_state_dir(),
            )
        });
    if let Err(detail) = result {
        eprintln!(
            "bosn: persistent maintenance registration failed: {detail}; the workspace daemon will still run, but maintenance after logout is not guaranteed. Retry `bosn daemon autostart enable`"
        );
    }
}

/// Refuse a daemon from another release rather than sending it requests it may
/// misread. It is never restarted here: it may be running another session's
/// jobs, so stopping it is the user's explicit choice.
fn matching_daemon(state_dir: &Path, daemon_version: &str) -> Result<(), String> {
    match bosn_service::daemon_version_mismatch(
        state_dir,
        env!("CARGO_PKG_VERSION"),
        daemon_version,
    ) {
        Some(mismatch) => Err(mismatch),
        None => Ok(()),
    }
}

/// Explain a daemon that would not start, including the one cutover trap: a
/// Python 0.1.x CLI keeps its (older-schema) registry in the same default
/// state directory, and the native daemon refuses to open it.
pub fn daemon_start_failure(state_dir: &Path) -> String {
    let mut message = format!(
        "the bosn daemon for {state} did not start; run `bosn daemon serve --state-dir {state}` to see why",
        state = state_dir.display()
    );
    if state_dir.join("registry.sqlite3").exists() {
        message.push_str(
            ". If this state directory belongs to the Python bosn 0.1.x CLI, stop that daemon (`bosn daemon stop` with the old CLI) and move the directory aside, or pass --state-dir; see docs/migration-rust.md",
        );
    }
    message
}

/// Keep the daemon out of this terminal's process group so Ctrl-C on `bosn
/// run` cancels the job without killing the daemon.
#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// The job id and how to cancel exactly this run, so a caller never has to
/// match `bosn run` processes by pattern (which also hits other sessions).
pub fn job_banner(job: u64, state_dir: &Path) -> String {
    format!(
        "bosn run: job {job}; cancel it with `bosn job cancel --state-dir {} --job-id {job}`",
        state_dir.display()
    )
}

/// Said once while a job waits for the daemon's job slot, so a queued run
/// is not mistaken for a hung one.
pub fn queued_notice(job: u64, state_dir: &Path) -> String {
    format!(
        "bosn run: job {job} is queued behind other jobs on the daemon for {}; it starts when they finish",
        state_dir.display()
    )
}

/// Stream one job's logs until it reaches a terminal state. Task stdout and
/// stderr go to this process's stdout and stderr (ensure output all goes to
/// stderr); daemon progress lines go to stderr. Ctrl-C cancels the job.
fn follow_job(
    runtime: &Runtime,
    client: &Client,
    state_dir: &Path,
    job: u64,
    task_output: bool,
) -> Result<JobStatus, String> {
    let mut interrupt = runtime
        .interrupt_signal()
        .map_err(|_| "cannot observe Ctrl-C".to_owned())?;
    let mut after = 0;
    let mut cancel_sent = false;
    let mut queued_reported = false;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    loop {
        let status = runtime
            .run(client.job_status(job))
            .map_err(|error| format!("lost contact with the bosn daemon: {error}"))?;
        if status.state == "Queued" && !queued_reported {
            eprintln!("{}", queued_notice(job, state_dir));
            queued_reported = true;
        }
        // Drain every retained page before deciding on termination so the
        // tail of the output is never dropped.
        loop {
            let page = runtime
                .run(client.job_logs(job, after, bosn_service::jobs::MAX_LOG_PAGE_RECORDS as u32))
                .map_err(|error| format!("lost contact with the bosn daemon: {error}"))?;
            if page.gap {
                eprintln!("bosn run: some job output was dropped before it could be read");
            }
            let fetched = page.records.len();
            for record in page.records {
                let (to_stdout, body, newline) = classify(&record.line, task_output);
                let result = if to_stdout {
                    let mut out = stdout.lock();
                    out.write_all(body.as_bytes())
                        .and_then(|()| {
                            if newline {
                                out.write_all(b"\n")
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|()| out.flush())
                } else {
                    let mut err = stderr.lock();
                    err.write_all(body.as_bytes())
                        .and_then(|()| {
                            if newline {
                                err.write_all(b"\n")
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|()| err.flush())
                };
                // A closed stdout (e.g. `| head`) must not wedge the job.
                if result.is_err() && !cancel_sent {
                    let _ = runtime.run(client.cancel_job(job));
                    cancel_sent = true;
                }
            }
            after = page.next;
            if fetched == 0 {
                break;
            }
        }
        if is_terminal(&status.state) {
            // Collect output written between the status read and the drain.
            let final_status = runtime
                .run(client.job_status(job))
                .map_err(|error| format!("lost contact with the bosn daemon: {error}"))?;
            let page = runtime
                .run(client.job_logs(job, after, bosn_service::jobs::MAX_LOG_PAGE_RECORDS as u32))
                .map_err(|error| format!("lost contact with the bosn daemon: {error}"))?;
            if page.records.is_empty() {
                return Ok(final_status);
            }
            continue;
        }
        let interrupted = runtime.run(async {
            async_engine::timeout(POLL_INTERVAL, interrupt.wait())
                .await
                .is_ok()
        });
        if interrupted && !cancel_sent {
            eprintln!("bosn run: interrupted; cancelling job {job}");
            let _ = runtime.run(client.cancel_job(job));
            cancel_sent = true;
        }
    }
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "Succeeded" | "Failed" | "Cancelled" | "Superseded")
}

/// Map one daemon log record to (stdout?, text, needs-newline). Engine
/// records are raw chunks tagged `[stdout] ` / `[stderr] ` and carry their
/// own newlines; everything else is a daemon progress line.
pub fn classify(line: &str, task_output: bool) -> (bool, &str, bool) {
    if let Some(body) = line.strip_prefix("[stdout] ") {
        (task_output, body, false)
    } else if let Some(body) = line.strip_prefix("[stderr] ") {
        (false, body, false)
    } else {
        (false, line, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<RunArguments, String> {
        parse_arguments(values.iter().map(OsString::from))
    }

    #[test]
    fn task_defaults_to_local_manifest_and_largest_output_budget() {
        let parsed = args(&["--task", "act-ci-static"]).unwrap();
        assert_eq!(parsed.task.as_deref(), Some("act-ci-static"));
        assert_eq!(parsed.stack, None);
        assert_eq!(parsed.manifest, None);
        assert_eq!(parsed.state_dir, None);
        assert_eq!(parsed.deadline, None);
        assert_eq!(parsed.output_limit, MANIFEST_MAX_OUTPUT);
        assert!(!parsed.no_ensure);
    }

    #[test]
    fn stack_only_is_an_ensure() {
        let parsed = args(&["--stack", "clud_act", "--manifest", "x/bosn.toml"]).unwrap();
        assert_eq!(parsed.task, None);
        assert_eq!(parsed.stack.as_deref(), Some("clud_act"));
        assert_eq!(parsed.manifest, Some(PathBuf::from("x/bosn.toml")));
    }

    #[test]
    fn refuses_missing_selector_duplicates_bad_names_and_out_of_range_budgets() {
        for bad in [
            &[][..],
            &["--task", "a", "--task", "b"],
            &["--task", "-a"],
            &["--task", "a b"],
            &["--stack", "s", "--no-ensure"],
            &["--task", "a", "--deadline-ms", "0"],
            &["--task", "a", "--deadline-ms", "14400001"],
            &["--task", "a", "--output-limit", "67108865"],
            &["--task", "a", "--bogus", "1"],
            &["--task"],
        ] {
            assert!(args(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(
            args(&["--task", "a", "--deadline-ms", "14400000"])
                .unwrap()
                .deadline,
            Some(MANIFEST_MAX_DEADLINE)
        );
    }

    const MANIFEST: &str = "[stack.one]\nimage = 'example.invalid/a@sha256:0000000000000000000000000000000000000000000000000000000000000000'\n[stack.two]\nimage = 'example.invalid/a@sha256:0000000000000000000000000000000000000000000000000000000000000000'\n[task.lint]\nstack = 'two'\ncmd = 'true'\n";

    #[test]
    fn resolves_the_stack_from_the_task_and_rejects_a_contradiction() {
        let workspace = Path::new("/workspace");
        assert_eq!(
            resolve_stack(MANIFEST, workspace, Some("lint"), None).unwrap(),
            "two"
        );
        assert_eq!(
            resolve_stack(MANIFEST, workspace, Some("lint"), Some("two")).unwrap(),
            "two"
        );
        assert!(resolve_stack(MANIFEST, workspace, Some("lint"), Some("one")).is_err());
        assert!(resolve_stack(MANIFEST, workspace, Some("absent"), None).is_err());
        assert_eq!(
            resolve_stack(MANIFEST, workspace, None, Some("one")).unwrap(),
            "one"
        );
        assert!(resolve_stack(MANIFEST, workspace, None, Some("three")).is_err());
    }

    #[test]
    fn workspace_is_the_manifest_directory() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let nested = temporary.path().join("repo");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("bosn.toml"), MANIFEST).unwrap();
        let (workspace, manifest) = manifest_location(&nested.join("bosn.toml")).unwrap();
        assert_eq!(workspace, std::fs::canonicalize(&nested).unwrap());
        assert_eq!(manifest, "bosn.toml");
        assert!(manifest_location(&nested.join("absent.toml")).is_err());
        let deeper = nested.join("a").join("b");
        std::fs::create_dir_all(&deeper).unwrap();
        assert_eq!(find_manifest(&deeper).unwrap(), nested.join("bosn.toml"));
        assert_eq!(find_manifest(&nested).unwrap(), nested.join("bosn.toml"));
        assert!(manifest_location(&nested).is_err());
    }

    #[test]
    fn recovers_the_task_exit_code_from_the_daemon_failure_text() {
        let failed = bosn_setup::SetupTaskError::TaskFailed {
            exit_code: 17,
            detail: "boom".into(),
        };
        assert_eq!(task_exit_code(&failed.to_string()), Some(17));
        assert_eq!(
            task_exit_code("declared manifest guest task exited with 3: no"),
            Some(3)
        );
        assert_eq!(
            task_exit_code("declared setup task exited with -1: signal"),
            Some(1)
        );
        assert_eq!(task_exit_code("manifest app task cancelled"), None);
    }

    #[test]
    fn a_daemon_that_will_not_start_points_at_the_legacy_state_trap_only_when_relevant() {
        let temporary = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let fresh = daemon_start_failure(temporary.path());
        assert!(fresh.contains("bosn daemon serve --state-dir"), "{fresh}");
        assert!(!fresh.contains("Python"), "{fresh}");
        std::fs::write(temporary.path().join("registry.sqlite3"), b"").unwrap();
        let legacy = daemon_start_failure(temporary.path());
        assert!(legacy.contains("Python bosn 0.1.x"), "{legacy}");
        assert!(legacy.contains("docs/migration-rust.md"), "{legacy}");
    }

    #[test]
    fn the_job_banner_names_this_run_and_its_exact_cancel_command() {
        let state = Path::new("/home/u/.local/state/bosn");
        assert_eq!(
            job_banner(87, state),
            "bosn run: job 87; cancel it with `bosn job cancel --state-dir /home/u/.local/state/bosn --job-id 87`"
        );
        assert!(queued_notice(87, state).contains("job 87 is queued"));
        // The follow lease must be accepted by the daemon and dwarf the poll.
        assert!(
            (bosn_service::FOLLOW_LEASE_MIN..=bosn_service::FOLLOW_LEASE_MAX)
                .contains(&FOLLOW_LEASE)
        );
        assert!(FOLLOW_LEASE >= POLL_INTERVAL * 100);
    }

    #[test]
    fn engine_records_keep_their_stream_and_progress_goes_to_stderr() {
        assert_eq!(classify("[stdout] a\n", true), (true, "a\n", false));
        assert_eq!(classify("[stdout] a\n", false), (false, "a\n", false));
        assert_eq!(classify("[stderr] b", true), (false, "b", false));
        assert_eq!(
            classify("[manifest-app-task] running", true),
            (false, "[manifest-app-task] running", true)
        );
    }
}
