//! Read-only planning for local GitHub Actions runs.
//!
//! `act` uses a Docker socket and creates resources behind Bosn's registry.
//! Execution must stay closed until Bosn owns an isolated engine lifecycle.

use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, RecvTimeoutError},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let Some(verb) = arguments.next() else {
        fail("expected plan, payload, run, or report")
    };
    if verb == "payload" {
        return payload(arguments);
    }
    if verb == "run" {
        fail(
            "act run requires isolated Docker ownership; nested Docker resources are not supervised",
        )
    }
    if verb == "report" {
        fail("act report requires a completed, supervised run")
    }
    if verb != "plan" {
        fail("expected plan, payload, run, or report")
    }
    let mut workspace = None;
    let mut workflow = None;
    let mut event = None;
    let mut mode = None;
    let mut sha = None;
    let mut version = None;
    let mut act_bin = None;
    let mut job = None;
    let mut json_output = false;
    while let Some(flag) = arguments.next() {
        let slot = match flag.to_str() {
            Some("--workspace") => &mut workspace,
            Some("--workflow") => &mut workflow,
            Some("--event") => &mut event,
            Some("--mode") => &mut mode,
            Some("--sha") => &mut sha,
            Some("--act-version") => &mut version,
            Some("--act-bin") => &mut act_bin,
            Some("--job") => &mut job,
            Some("--json") if !json_output => {
                json_output = true;
                continue;
            }
            _ => fail("invalid or duplicate act option"),
        };
        if slot.is_some() {
            fail("duplicate act option")
        }
        *slot = Some(
            arguments
                .next()
                .and_then(|v| v.into_string().ok())
                .unwrap_or_else(|| fail("missing act option value")),
        );
    }
    let workspace = workspace.unwrap_or_else(|| fail("--workspace is required"));
    let workflow = workflow.unwrap_or_else(|| fail("--workflow is required"));
    let event = event.unwrap_or_else(|| fail("--event is required"));
    let mode = mode.unwrap_or_else(|| fail("--mode is required"));
    let sha = sha.unwrap_or_else(|| fail("--sha is required"));
    let version = version.unwrap_or_else(|| fail("--act-version is required"));
    if !matches!(event.as_str(), "pull_request" | "push" | "release") {
        fail("invalid event")
    }
    if !matches!(mode.as_str(), "minimal" | "test" | "full") {
        fail("invalid mode")
    }
    if event == "release" && mode != "full" {
        fail("release requires full mode")
    }
    if event == "push" && mode != "minimal" {
        fail("push requires minimal mode; full validation requires a release event")
    }
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        fail("--sha must be 40 hexadecimal characters")
    }
    if version.is_empty()
        || version.len() > 32
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        fail("invalid act version")
    }
    if let Some(ref job) = job {
        if job.is_empty()
            || job.len() > 128
            || !job
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            fail("invalid job ID")
        }
    }
    let root = PathBuf::from(workspace)
        .canonicalize()
        .unwrap_or_else(|_| fail("workspace does not exist"));
    let top_level = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .unwrap_or_else(|_| fail("workspace Git root is unavailable"));
    if !top_level.status.success() {
        fail("workspace Git root is unavailable")
    }
    let git_root = String::from_utf8(top_level.stdout)
        .unwrap_or_else(|_| fail("workspace Git root is invalid"));
    let git_root = PathBuf::from(git_root.trim())
        .canonicalize()
        .unwrap_or_else(|_| fail("workspace Git root is unavailable"));
    if git_root != root {
        fail("--workspace must be the Git checkout root")
    }
    let relative = Path::new(&workflow);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        fail("workflow must be a relative path without traversal")
    }
    let path = root
        .join(relative)
        .canonicalize()
        .unwrap_or_else(|_| fail("workflow does not exist"));
    if !path.starts_with(&root) || !path.is_file() {
        fail("workflow escapes workspace or is not a file")
    }
    let head = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|_| fail("workspace Git HEAD is unavailable"));
    if !head.status.success() {
        fail("workspace Git HEAD is unavailable")
    }
    let actual_sha =
        String::from_utf8(head.stdout).unwrap_or_else(|_| fail("workspace Git HEAD is invalid"));
    if actual_sha.trim() != sha.to_ascii_lowercase() {
        fail("requested SHA does not match workspace HEAD")
    }
    let head_path = format!("HEAD:{workflow}");
    let committed = Command::new("git")
        .current_dir(&root)
        .args(["show", &head_path])
        .output()
        .unwrap_or_else(|_| fail("workflow is not in workspace HEAD"));
    if !committed.status.success() {
        fail("workflow is not in workspace HEAD")
    }
    let current = std::fs::read(&path).unwrap_or_else(|_| fail("workflow could not be read"));
    if current != committed.stdout {
        fail("workflow differs from workspace HEAD")
    }
    let workspace_status = Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap_or_else(|_| fail("workspace source status is unavailable"));
    if !workspace_status.status.success() || !workspace_status.stdout.is_empty() {
        fail("workspace source differs from workspace HEAD")
    }
    let index_flags = Command::new("git")
        .current_dir(&root)
        .args(["ls-files", "-v", "-z"])
        .output()
        .unwrap_or_else(|_| fail("workspace index flags are unavailable"));
    if !index_flags.status.success()
        || index_flags
            .stdout
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .any(|record| record[0].is_ascii_lowercase() || record[0] == b'S')
    {
        fail("workspace index hides tracked file changes")
    }
    // The generic Bosn surface does not assume each repository's selector or
    // dispatch input names. Its event mapping is an intention until the repo's
    // adapter has furnished and validated the exact event payload.
    let github_event = if event == "release" {
        "workflow_dispatch"
    } else {
        &event
    };
    let act_bin = act_bin.unwrap_or_else(|| "act".to_owned());
    let version_line = String::from_utf8(bounded_act_output(
        Command::new(&act_bin).current_dir(&root).arg("--version"),
        Duration::from_secs(2),
        4096,
    ))
    .unwrap_or_else(|_| fail("act version output is not UTF-8"));
    if version_line.trim() != format!("act version {version}") {
        fail("act binary does not match --act-version")
    }
    let output = String::from_utf8(bounded_act_output(
        Command::new(&act_bin)
            .current_dir(&root)
            .args(["-l", "-W", &workflow]),
        Duration::from_secs(2),
        1024 * 1024,
    ))
    .unwrap_or_else(|_| fail("act list output is not UTF-8"));
    let jobs = parse_list(&output);
    let final_head = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|_| fail("workspace Git HEAD changed during planning"));
    let final_status = Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .unwrap_or_else(|_| fail("workspace source changed during planning"));
    if !final_head.status.success()
        || String::from_utf8_lossy(&final_head.stdout).trim() != actual_sha.trim()
        || !final_status.status.success()
        || !final_status.stdout.is_empty()
    {
        fail("workspace changed during planning")
    }
    let selected_jobs: Vec<&Value> = jobs
        .iter()
        .filter(|row| job.as_ref().is_none_or(|wanted| row["id"] == *wanted))
        .collect();
    if selected_jobs.is_empty() {
        fail("selected job does not exist in act list")
    }
    let result = json!({"action":"act_plan", "schema_version":1, "workspace":root,
        "workflow":relative, "event":event, "github_event":github_event,
        "mode":mode, "sha":sha.to_ascii_lowercase(), "act_version":version,
        "act_version_source":"caller", "binary_version_verified":true, "fleet_pin_verified":false,
        "event_payload_resolved":false, "source_clean_scope":"Git HEAD and status checked before and after act queries; hidden index flags rejected; ignored files and later changes unchecked",
        "job":job, "jobs":jobs, "selected_jobs":selected_jobs,
        "selection_scope":"workflow declarations; event, mode, matrix, and job if conditions are unresolved",
        "executable":false, "docker_resources_tracked":false,
        "reason":"repository event adapter and isolated Docker ownership are required"});
    if json_output {
        println!("{result}")
    } else {
        println!("act plan: {} {} {} at {}", workflow, event, mode, sha);
        for row in &selected_jobs {
            println!(
                "stage {}: {}",
                row["stage"],
                row["id"].as_str().unwrap_or("")
            );
        }
        println!(
            "execution unavailable: repository event adapter and isolated Docker ownership are required"
        );
    }
}

/// Bosn's checked-in CI selector consumes these exact GitHub event fields.
/// This adapter deliberately supports one repository and leaves execution closed.
fn payload(mut arguments: impl Iterator<Item = OsString>) {
    let mut event = None;
    let mut mode = None;
    let mut sha = None;
    let mut pr_number = None;
    while let Some(flag) = arguments.next() {
        let slot = match flag.to_str() {
            Some("--event") => &mut event,
            Some("--mode") => &mut mode,
            Some("--sha") => &mut sha,
            Some("--pr-number") => &mut pr_number,
            _ => fail("invalid or duplicate payload option"),
        };
        if slot.is_some() {
            fail("duplicate payload option")
        }
        *slot = Some(
            arguments
                .next()
                .and_then(|v| v.into_string().ok())
                .unwrap_or_else(|| fail("missing payload option value")),
        );
    }
    let event = event.unwrap_or_else(|| fail("--event is required"));
    let mode = mode.unwrap_or_else(|| fail("--mode is required"));
    let sha = sha.unwrap_or_else(|| fail("--sha is required"));
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        fail("--sha must be 40 hexadecimal characters")
    }
    let sha = sha.to_ascii_lowercase();
    let (github_event, payload) = match (event.as_str(), mode.as_str()) {
        ("pull_request", "minimal" | "test" | "full") => {
            let number = pr_number
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or_else(|| fail("pull_request requires positive --pr-number"));
            let labels: Vec<&str> = match mode.as_str() {
                "test" => vec!["ci-test"],
                "full" => vec!["ci-full"],
                _ => vec![],
            };
            (
                "pull_request",
                json!({"action":"synchronize","number":number,
                "repository":{"full_name":"zackees/bosn"},
                "pull_request":{"number":number,"head":{"sha":sha},
                    "labels":labels.iter().map(|name| json!({"name":name})).collect::<Vec<_>>()}}),
            )
        }
        ("push", "minimal") if pr_number.is_none() => (
            "push",
            json!({
            "ref":"refs/heads/main", "after":sha,
            "repository":{"full_name":"zackees/bosn"}}),
        ),
        ("release", "full") if pr_number.is_none() => (
            "workflow_dispatch",
            json!({
            "ref":"refs/heads/main", "inputs":{"tier":"full","commit_sha":sha},
            "repository":{"full_name":"zackees/bosn"}}),
        ),
        _ => fail("unsupported event/mode combination or unexpected --pr-number"),
    };
    println!(
        "{}",
        json!({"schema_version":1,"repository":"zackees/bosn",
        "workflow":".github/workflows/ci.yml","github_event":github_event,
        "mode":mode,"sha":sha,"payload":payload,"executable":false,
        "reason":"event adapter only; isolated Docker engine and job coverage report are required"})
    );
}

/// Run only the two non-executing Act queries. Pipe readers drain concurrently
/// so neither stdout nor stderr can block the child before the deadline.
fn bounded_act_output(command: &mut Command, deadline: Duration, limit: usize) -> Vec<u8> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|_| fail("act executable is unavailable"));
    let (tx, rx) = mpsc::sync_channel::<Option<(bool, Vec<u8>)>>(8);
    for (is_stdout, mut pipe) in [
        (
            true,
            Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        ),
        (
            false,
            Box::new(child.stderr.take().unwrap()) as Box<dyn Read + Send>,
        ),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Some((is_stdout, chunk[..n].to_vec()))).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(tx);
    let start = Instant::now();
    let mut stdout = Vec::new();
    let mut used = 0_usize;
    let mut eof = 0;
    while eof < 2 {
        let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
            let _ = child.kill();
            let _ = child.wait();
            fail("act query timed out")
        };
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(None) => eof += 1,
            Ok(Some((is_stdout, bytes))) => {
                used = used.saturating_add(bytes.len());
                if used > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    fail("act query output limit exceeded")
                }
                if is_stdout {
                    stdout.extend_from_slice(&bytes)
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => fail("act query output ended unexpectedly"),
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                fail("act query timed out")
            }
            Err(_) => fail("act query could not be reaped"),
        }
    };
    if !status.success() {
        fail("act query failed")
    }
    stdout
}

fn parse_list(output: &str) -> Vec<Value> {
    let mut lines = output.lines();
    let header = lines
        .next()
        .unwrap_or_else(|| fail("act list has no header"));
    if !header.contains("Stage") || !header.contains("Job ID") {
        fail("act list header is not recognized")
    }
    let mut jobs = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let stage: u32 = fields
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| fail("act list stage is invalid"));
        let id = fields
            .next()
            .unwrap_or_else(|| fail("act list job ID is missing"));
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            fail("act list job ID is invalid")
        }
        jobs.push(json!({"stage":stage,"id":id}));
    }
    if jobs.is_empty() {
        fail("act list contains no jobs")
    }
    jobs
}

fn fail(message: &str) -> ! {
    eprintln!("bosn act: {message}");
    std::process::exit(2)
}
