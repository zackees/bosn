//! `bosn jobs`: the daemon's runner accounting (#358).
//!
//! One line per queued, running or recently finished job: its lane and
//! runner slot, how long it has been idle, and for a running task the CPU
//! allocation, its setup container and the containers it created, which are
//! found by their `com.zackees.bosn.run` label.

use std::{
    ffi::OsString,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use bosn_service::{
    Client,
    docker_api::{DockerApi, LABEL_RUN},
};
use kernal_api::async_engine::RuntimeBuilder;
use serde_json::Value;
use std::fmt::Write as _;

pub const USAGE: &str = "bosn jobs [--state-dir STATE_DIR] [--json]";

pub fn run(mut arguments: impl Iterator<Item = OsString>) {
    let mut state_dir: Option<PathBuf> = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" if state_dir.is_none() => match arguments.next() {
                Some(value) if !value.is_empty() => state_dir = Some(PathBuf::from(value)),
                _ => fail(),
            },
            "--json" if !json => json = true,
            _ => fail(),
        }
    }
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let view = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .ok()
        .and_then(|runtime| {
            let client = Client::for_state(&state_dir).ok()?;
            runtime.run(client.jobs()).ok()
        });
    let Some(mut view) = view else {
        if json {
            println!(
                "{}",
                serde_json::json!({"action": "jobs", "daemon": "unavailable"})
            );
        } else {
            eprintln!(
                "bosn jobs: no daemon answered for {} (or it predates `bosn jobs`)",
                state_dir.display()
            );
        }
        std::process::exit(1);
    };
    attach_containers(&mut view);
    if json {
        emit(&format!("{view}\n"));
    } else {
        print_table(&view);
    }
}

/// Write the whole report at once. A closed pipe (`bosn jobs | head`) is not
/// an error worth a panic.
fn emit(text: &str) {
    use std::io::Write;
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

fn fail() -> ! {
    eprintln!("usage: {USAGE}");
    std::process::exit(2);
}

/// Add each running task's live containers (by run label) to its entry.
fn attach_containers(view: &mut Value) {
    let Some(api) = DockerApi::from_environment() else {
        return;
    };
    let Some(jobs) = view.get_mut("jobs").and_then(Value::as_array_mut) else {
        return;
    };
    for job in jobs {
        let Some(run) = job
            .pointer("/run/run")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if let Ok(ids) = api.labelled("containers", LABEL_RUN, Some(&run)) {
            job["run"]["containers"] = Value::from(
                ids.iter()
                    .map(|id| id[..id.len().min(12)].to_owned())
                    .collect::<Vec<_>>(),
            );
        }
    }
}

#[expect(clippy::too_many_lines, reason = "baseline, ci.yml#229")]
fn print_table(view: &Value) {
    let mut out = String::new();
    let capacity = &view["capacity"];
    let cpus = capacity["cpus_per_slot"]
        .as_f64()
        .filter(|c| *c > 0.0)
        .map_or_else(|| "unlimited CPUs".to_owned(), |c| format!("{c} CPUs"));
    let _ = writeln!(
        out,
        "runner slots: {} x {cpus} (host {} CPUs), control slots: {}, stall teardown: {}, docker proxy: {}",
        capacity["runner_slots"],
        capacity["host_cpus"],
        capacity["control_slots"],
        capacity["stall_seconds"]
            .as_u64()
            .map_or_else(|| "off".to_owned(), |s| format!("{s}s")),
        if capacity["docker_proxy"].as_bool() == Some(true) {
            "on"
        } else {
            "off"
        },
    );
    for lane in ["runner", "control"] {
        let _ = writeln!(
            out,
            "{lane:>7}: {} running, {} queued",
            view["lanes"][lane]["running"], view["lanes"][lane]["queued"]
        );
    }
    let jobs = view["jobs"].as_array().cloned().unwrap_or_default();
    if jobs.is_empty() {
        let _ = writeln!(out, "no jobs");
        emit(&out);
        return;
    }
    let _ = writeln!(
        out,
        "{:>5}  {:<10}  {:<7}  {:>4}  {:>7}  {:>6}  {:<28}  WORKSPACE / CONTAINERS",
        "ID", "STATE", "LANE", "SLOT", "AGE", "IDLE", "KEY"
    );
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    for job in jobs {
        let age = job["started_ms"]
            .as_u64()
            .or(job["submitted_ms"].as_u64())
            .map(|start| {
                let end = job["finished_ms"].as_u64().unwrap_or(now_ms);
                duration(end.saturating_sub(start) / 1000)
            })
            .unwrap_or_default();
        let idle = job["idle_seconds"]
            .as_u64()
            .map(duration)
            .unwrap_or_default();
        let slot = job["slot"]
            .as_u64()
            .map_or_else(|| "-".to_owned(), |s| (s + 1).to_string());
        let _ = writeln!(
            out,
            "{:>5}  {:<10}  {:<7}  {:>4}  {:>7}  {:>6}  {:<28}  {}",
            job["id"].to_string(),
            job["state"].as_str().unwrap_or("?"),
            job["class"].as_str().unwrap_or("?"),
            slot,
            age,
            idle,
            truncate(job["key"].as_str().unwrap_or(""), 28),
            job["workspace"].as_str().unwrap_or(""),
        );
        if let Some(run) = job.get("run") {
            let containers = run["containers"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "{:>5}  task {} in {}; run {}; docker: {} requests, {} creates; live containers: [{}]; caches: {}",
                "",
                run["task"].as_str().unwrap_or(""),
                run["container"].as_str().unwrap_or("-"),
                run["run"].as_str().unwrap_or(""),
                run["docker_requests"],
                run["docker_creates"],
                containers,
                run["caches"]
                    .as_array()
                    .map(|c| c
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" "))
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| "-".into()),
            );
        }
        if let Some(error) = job["error"].as_str() {
            let _ = writeln!(out, "{:>5}  error: {}", "", truncate(error, 160));
        }
    }
    emit(&out);
}

fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        let mut text: String = value.chars().take(width.saturating_sub(1)).collect();
        text.push('~');
        text
    }
}
