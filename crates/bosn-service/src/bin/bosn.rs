//! Native Bosn command entry point.
//!
//! Python packaging invokes the same `bosn-service::mcp::serve_stdio` function
//! through PyO3 today. This binary makes `cargo run -p bosn-service --bin bosn
//! -- mcp` an equivalent, package-ready route without a Python launcher.  Its
//! setup route is a separate human/JSON CLI and never shares MCP stdio.

use std::time::Duration;
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use bosn_core::{ResourceKind, parse_and_plan_compose_yaml, parse_setup_config_locator};
use bosn_engine::{DockerEngine, RunOptions};
use bosn_service::{
    Client, JobLogPage, JobStatus, ManifestAppTaskJobRequest, ManifestConvergeJobRequest,
    ManifestEnsureJobRequest, PythonV4ObservedResource, PythonV4ReconcileExecutor,
    SetupEnsureJobRequest, SetupPreparePolicy, SetupPrepareRequest, SetupTaskJobRequest,
    apply_python_v4_reconciliation, preview_python_v4_reconciliation,
};
use bosn_setup::{
    SetupAcquirePolicy, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupSourceKind,
    plan_setup,
};
use kernal_api::{
    async_engine::RuntimeBuilder,
    platform::{fs, ipc},
};
use serde_json::json;

#[path = "bosn/act.rs"]
mod act;
#[path = "bosn/args.rs"]
mod args;
#[path = "bosn/bounded_output.rs"]
mod bounded_output;
#[path = "bosn/ci.rs"]
mod ci;
#[path = "bosn/daemon.rs"]
mod daemon;
#[path = "bosn/gc.rs"]
mod gc;
#[path = "bosn/health.rs"]
mod health;
#[path = "bosn/job.rs"]
mod job;
#[path = "bosn/manifest.rs"]
mod manifest;
#[path = "bosn/registry.rs"]
mod registry;
#[path = "bosn/run.rs"]
mod run;
#[path = "bosn/scan.rs"]
mod scan;
#[path = "bosn/secret.rs"]
mod secret;
#[path = "bosn/setup.rs"]
mod setup;
#[path = "bosn/setup_args.rs"]
mod setup_args;
#[path = "bosn/widget.rs"]
mod widget;
use args::*;
use daemon::*;
use gc::*;
use health::*;
use job::*;
use manifest::*;
use registry::*;
use scan::*;
use setup::*;
use setup_args::*;

const SETUP_PREPARE_MAX_DEADLINE_MS: u64 = 5 * 60 * 1_000;
const SETUP_PREPARE_MAX_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
const MANIFEST_MAX_DEADLINE_MS: u64 = bosn_service::MANIFEST_MAX_DEADLINE.as_millis() as u64;
const MANIFEST_MAX_OUTPUT_LIMIT: usize = bosn_service::MANIFEST_MAX_OUTPUT;
const DEFAULT_JOB_LOG_LIMIT: u32 = 64;
const MAX_JOB_LOG_LIMIT: u32 = bosn_service::jobs::MAX_LOG_PAGE_RECORDS as u32;
/// The CLI may explicitly read one local Compose file, but it never executes
/// it and keeps the read bounded before parsing.
const MAX_COMPOSE_FILE_BYTES: usize = 1024 * 1024;

fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let Some(command) = arguments.next() else {
        usage();
    };
    if command == "--version" || command == "-V" {
        println!("bosn {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    match command.to_string_lossy().as_ref() {
        "mcp" => run_mcp(arguments),
        "daemon" => run_daemon(arguments),
        "doctor" => run_doctor(arguments),
        "compose" => run_compose(arguments),
        "act" => act::run(arguments),
        "ci" => ci::run(arguments, None),
        "ui" => ci::run_ui(arguments),
        "widget" => widget::run(arguments),
        "manifest" => run_manifest(arguments),
        "run" => run::run(arguments),
        "setup" => run_setup(arguments),
        "job" => run_job(arguments),
        "registry" => run_registry(arguments),
        "gc" => run_gc(arguments),
        "scan" => run_scan(arguments),
        "secret" => secret::run(arguments),
        _ => usage(),
    }
}

fn run_compose(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    match arguments.next().as_deref() {
        Some(command) if command == "plan" => run_compose_plan(arguments),
        _ => usage(),
    }
}

/// Read and plan an explicitly selected Compose file.  This intentionally has
/// no daemon, registry, Docker, or workspace dependency.
fn run_compose_plan(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut file = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--file" => set_once(&mut file, arguments.next()),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let file = PathBuf::from(file.unwrap_or_else(|| usage()));
    let source = read_compose_file(&file).unwrap_or_else(|error| compose_failure(&error));
    let plan = parse_and_plan_compose_yaml(&source)
        .unwrap_or_else(|error| compose_failure(&error.to_string()));
    if json_output {
        println!(
            "{}",
            json!({
                "action": "compose_plan",
                "applied": false,
                "version": plan.version,
                "digest": plan.digest,
                "document": plan.document,
                "normalized_json": plan.normalized_json,
            })
        );
    } else {
        println!("compose plan (not applied)");
        println!("version: {}", plan.version);
        println!("digest: {}", plan.digest);
        println!(
            "services: {}",
            plan.document
                .services
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn read_compose_file(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|_| "could not open Compose file".to_owned())?;
    let mut bytes = Vec::with_capacity(MAX_COMPOSE_FILE_BYTES.saturating_add(1));
    file.take((MAX_COMPOSE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read Compose file".to_owned())?;
    if bytes.len() > MAX_COMPOSE_FILE_BYTES {
        return Err(format!(
            "Compose file exceeds the {MAX_COMPOSE_FILE_BYTES}-byte limit"
        ));
    }
    String::from_utf8(bytes).map_err(|_| "Compose file is not valid UTF-8".to_owned())
}

fn compose_failure(error: &str) -> ! {
    eprintln!("bosn compose plan: {error}");
    std::process::exit(1)
}

fn usage() -> ! {
    eprintln!(
        "   or: bosn act plan --workspace WORKSPACE --workflow RELATIVE_YML --event pull_request|push|release --mode minimal|test|full --sha 40_HEX --act-version VERSION [--act-bin PATH] [--job ID] [--json]"
    );
    eprintln!(
        "   or: bosn act run|report|plan --adapter (deprecated aliases of bosn ci run|report|plan --adapter)"
    );
    eprintln!("   or: {}", ci::USAGE.trim_start_matches("usage: "));
    eprintln!("   or: {}", widget::USAGE.trim_start_matches("usage: "));
    eprintln!("usage: bosn mcp [--state-dir STATE_DIR]");
    eprintln!("   or: {}", run::USAGE.trim_start_matches("usage: "));
    eprintln!(
        "   or: bosn secret set github_token [--from-gh] [--state-dir STATE_DIR] (value on stdin)\n   or: bosn secret status [--state-dir STATE_DIR] [--json]\n   or: bosn secret remove github_token [--state-dir STATE_DIR]"
    );
    eprintln!("   or: bosn daemon serve --state-dir STATE_DIR");
    eprintln!("   or: bosn daemon status --state-dir STATE_DIR [--json]");
    eprintln!("   or: bosn daemon stop --state-dir STATE_DIR [--json]");
    eprintln!(
        "   or: bosn daemon autostart (enable|disable|status) [--state-dir STATE_DIR] [--json]"
    );
    eprintln!(
        "   or: bosn registry import-v4 --legacy-state-dir LEGACY_STATE_DIR --state-dir NEW_STATE_DIR --yes [--json]"
    );
    eprintln!(
        "   or: bosn registry reconcile-v4 preview --state-dir STATE_DIR [--json]\n   or: bosn registry reconcile-v4 apply --state-dir STATE_DIR --apply --yes [--json]"
    );
    eprintln!("   or: bosn compose plan --file COMPOSE_YAML [--json]");
    eprintln!(
        "   or: bosn setup plan --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) [--json]"
    );
    eprintln!(
        "   or: bosn setup prepare --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    eprintln!(
        "   or: bosn setup task --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) --task NAME --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    eprintln!(
        "   or: bosn setup app-task --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) --task NAME --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    eprintln!(
        "   or: bosn manifest app-task --state-dir STATE_DIR --workspace WORKSPACE --manifest RELATIVE_TOML --stack NAME --task NAME --deadline-ms 1..=14400000 --output-limit 1..=67108864 [--json]"
    );
    eprintln!(
        "   or: bosn manifest ensure --state-dir STATE_DIR --workspace WORKSPACE --manifest RELATIVE_TOML --stack NAME --deadline-ms 1..=14400000 --output-limit 1..=67108864 [--json]"
    );
    eprintln!(
        "   or: bosn manifest converge --state-dir STATE_DIR --workspace WORKSPACE --manifest RELATIVE_TOML --deadline-ms 1..=14400000 --output-limit 1..=67108864 [--json]"
    );
    eprintln!(
        "   or: bosn setup ensure --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    eprintln!(
        "   or: bosn setup reconcile preview --state-dir STATE_DIR --workspace WORKSPACE [--after CURSOR] [--limit 1..=64] [--json]\n   or: bosn setup reconcile repair-missing --state-dir STATE_DIR --workspace WORKSPACE --candidate TOKEN --apply --yes [--json]"
    );
    eprintln!("   or: bosn setup done --state-dir STATE_DIR --workspace WORKSPACE --yes [--json]");
    eprintln!(
        "   or: bosn setup stop-retired --state-dir STATE_DIR --workspace WORKSPACE --candidate TOKEN --apply --yes [--json]"
    );
    eprintln!("   or: bosn job status --state-dir STATE_DIR --job-id ID [--json]");
    eprintln!(
        "   or: bosn job logs --state-dir STATE_DIR --job-id ID [--after CURSOR] [--limit 1..={MAX_JOB_LOG_LIMIT}] [--json]"
    );
    eprintln!("   or: bosn job cancel --state-dir STATE_DIR --job-id ID [--json]");
    eprintln!("   or: bosn scan [--state-dir STATE_DIR] [--ttl-seconds N] [--ack] [--json]");
    eprintln!(
        "   or: bosn gc --unmanaged [--state-dir STATE_DIR] [--ttl-seconds N] [--include ID]... [--apply --yes] [--json]"
    );
    eprintln!(
        "   or: bosn gc preview --state-dir STATE_DIR --workspace WORKSPACE [--after CURSOR] [--limit 1..=64] [--json]"
    );
    eprintln!(
        "   or: bosn gc apply --state-dir STATE_DIR --workspace WORKSPACE --candidate TOKEN --apply --yes [--json]"
    );
    eprintln!(
        "   or: bosn manifest volume-gc preview --state-dir STATE_DIR --workspace WORKSPACE [--after CURSOR] [--limit 1..=64] [--json]\n   or: bosn manifest volume-gc apply --state-dir STATE_DIR --workspace WORKSPACE --candidate TOKEN --apply --yes [--json]"
    );
    eprintln!(
        "   or: bosn manifest volume-release preview --state-dir STATE_DIR --workspace WORKSPACE [--after CURSOR] [--limit 1..=64] [--json]\n   or: bosn manifest volume-release apply --state-dir STATE_DIR --workspace WORKSPACE --candidate TOKEN --apply --yes [--json]"
    );
    std::process::exit(2)
}
