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

const SETUP_PREPARE_MAX_DEADLINE_MS: u64 = 5 * 60 * 1_000;
const SETUP_PREPARE_MAX_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
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
        "manifest" => run_manifest(arguments),
        "setup" => run_setup(arguments),
        "job" => run_job(arguments),
        "registry" => run_registry(arguments),
        "gc" => run_gc(arguments),
        "scan" => run_scan(arguments),
        _ => usage(),
    }
}

fn run_manifest(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    match arguments.next().as_deref() {
        Some(command) if command == "ensure" => run_manifest_ensure(arguments),
        Some(command) if command == "converge" => run_manifest_converge(arguments),
        Some(command) if command == "app-task" => run_manifest_app_task(arguments),
        Some(command) if command == "volume-gc" => run_manifest_volume_gc(arguments),
        Some(command) if command == "volume-release" => run_manifest_volume_release(arguments),
        _ => usage(),
    }
}

fn run_manifest_volume_release(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(verb) = arguments.next() else {
        usage();
    };
    if verb.as_os_str() == std::ffi::OsStr::new("preview") {
        let (state_dir, workspace, after, limit, json_output) =
            parse_gc_preview_arguments(arguments).unwrap_or_else(|_| usage());
        let result = Client::for_state(state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_release_preview(workspace, after, limit))
                        .ok()
                })
        });
        match result {
            Some(page) => println!(
                "{}",
                json!({"action":"manifest_volume_release_preview","preview_only":true,"next":page.next,"candidates":page.candidates.into_iter().map(|v|json!({"id":v.id,"name":v.name,"generation":v.generation,"token":v.token,"reason":v.reason})).collect::<Vec<_>>() })
            ),
            None => gc_failure(json_output),
        }
    } else if verb.as_os_str() == std::ffi::OsStr::new("apply") {
        let mut state_dir = None;
        let mut workspace = None;
        let mut token = None;
        let mut apply = false;
        let mut yes = false;
        let mut json_output = false;
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
                "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
                "--candidate" => set_once_parsed(&mut token, arguments.next(), |v| {
                    v.to_str().map(str::to_owned).ok_or(())
                }),
                "--apply" if !apply => {
                    apply = true;
                    Ok(())
                }
                "--yes" if !yes => {
                    yes = true;
                    Ok(())
                }
                "--json" if !json_output => {
                    json_output = true;
                    Ok(())
                }
                _ => Err(()),
            }
            .unwrap_or_else(|_| usage());
        }
        let (Some(state_dir), Some(workspace), Some(token)) = (state_dir, workspace, token) else {
            usage();
        };
        if !apply || !yes {
            usage();
        }
        let result = Client::for_state(state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_release_apply(workspace, &token, true))
                        .ok()
                })
        });
        match result {
            Some(value) => println!(
                "{}",
                json!({"action":"manifest_volume_release_apply","removed":value.removed,"reconciled_missing":value.reconciled_missing})
            ),
            None => gc_failure(json_output),
        }
    } else {
        usage();
    }
}

fn run_manifest_volume_gc(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(verb) = arguments.next() else {
        usage();
    };
    if verb.as_os_str() == std::ffi::OsStr::new("preview") {
        let (state_dir, workspace, after, limit, json_output) =
            parse_gc_preview_arguments(arguments).unwrap_or_else(|_| usage());
        let result = Client::for_state(state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_gc_preview(workspace, after, limit))
                        .ok()
                })
        });
        match result {
            Some(page) => println!(
                "{}",
                json!({"action":"manifest_volume_gc_preview","preview_only":true,"next":page.next,"candidates":page.candidates.into_iter().map(|v|json!({"id":v.id,"name":v.name,"generation":v.generation,"token":v.token,"reason":v.reason})).collect::<Vec<_>>(),"counts":{"protected_not_retired":page.counts.protected_not_retired,"protected_policy":page.counts.protected_policy,"protected_ambiguous_use":page.counts.protected_ambiguous_use,"protected_lease":page.counts.protected_lease,"protected_session":page.counts.protected_session,"protected_intent":page.counts.protected_intent,"excluded_unmanaged":page.counts.excluded_unmanaged}})
            ),
            None => gc_failure(json_output),
        }
    } else if verb.as_os_str() == std::ffi::OsStr::new("apply") {
        let mut state_dir = None;
        let mut workspace = None;
        let mut token = None;
        let mut apply = false;
        let mut yes = false;
        let mut json_output = false;
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
                "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
                "--candidate" => set_once_parsed(&mut token, arguments.next(), |v| {
                    v.to_str().map(str::to_owned).ok_or(())
                }),
                "--apply" if !apply => {
                    apply = true;
                    Ok(())
                }
                "--yes" if !yes => {
                    yes = true;
                    Ok(())
                }
                "--json" if !json_output => {
                    json_output = true;
                    Ok(())
                }
                _ => Err(()),
            }
            .unwrap_or_else(|_| usage());
        }
        let (Some(state_dir), Some(workspace), Some(token)) = (state_dir, workspace, token) else {
            usage();
        };
        if !apply || !yes {
            usage();
        }
        let result = Client::for_state(state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_gc_apply(workspace, &token, true))
                        .ok()
                })
        });
        match result {
            Some(value) => println!(
                "{}",
                json!({"action":"manifest_volume_gc_apply","removed":value.removed,"reconciled_missing":value.reconciled_missing})
            ),
            None => gc_failure(json_output),
        }
    } else {
        usage();
    }
}

/// Converge every declared manifest stack in the daemon's deterministic
/// lexical order. This accepts no dependency, root, Docker, or stack selector
/// because the legacy TOML schema does not represent dependency edges.
fn run_manifest_converge(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut manifest = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--manifest" => set_once_parsed(&mut manifest, arguments.next(), parse_manifest_path),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let request = ManifestConvergeJobRequest {
        workspace: PathBuf::from(workspace.unwrap_or_else(|| usage())),
        manifest: manifest.unwrap_or_else(|| usage()),
        deadline: Duration::from_millis(deadline_ms.unwrap_or_else(|| usage())),
        output_limit: output_limit.unwrap_or_else(|| usage()),
    };
    let state_dir = state_dir.unwrap_or_else(|| usage());
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| usage());
    let client = Client::for_state(state_dir).unwrap_or_else(|_| usage());
    match runtime.run(client.submit_manifest_converge(request)) {
        Ok(job_id) if json_output => println!(
            "{}",
            json!({"action":"manifest_converge","submitted":true,"job_id":job_id})
        ),
        Ok(job_id) => println!("manifest converge submitted: {job_id}"),
        Err(_) => usage(),
    }
}

/// Submit a declared task for an already ensured supported manifest stack.
/// No command, container, image, or Docker controls exist on this surface.
fn run_manifest_app_task(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut manifest = None;
    let mut stack = None;
    let mut task_name = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--manifest" => set_once_parsed(&mut manifest, arguments.next(), parse_manifest_path),
            "--stack" => set_once_parsed(&mut stack, arguments.next(), parse_setup_task_name),
            "--task" => set_once_parsed(&mut task_name, arguments.next(), parse_setup_task_name),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let request = ManifestAppTaskJobRequest {
        workspace: PathBuf::from(workspace.unwrap_or_else(|| usage())),
        manifest: manifest.unwrap_or_else(|| usage()),
        stack: stack.unwrap_or_else(|| usage()),
        task_name: task_name.unwrap_or_else(|| usage()),
        deadline: Duration::from_millis(deadline_ms.unwrap_or_else(|| usage())),
        output_limit: output_limit.unwrap_or_else(|| usage()),
    };
    let state_dir = state_dir.unwrap_or_else(|| usage());
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| usage());
    let client = Client::for_state(state_dir).unwrap_or_else(|_| usage());
    match runtime.run(client.submit_manifest_app_task(request)) {
        Ok(job_id) if json_output => println!(
            "{}",
            json!({"action":"manifest_app_task","submitted":true,"job_id":job_id})
        ),
        Ok(job_id) => println!("manifest app task submitted: {job_id}"),
        Err(_) => usage(),
    }
}

fn run_manifest_ensure(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut manifest = None;
    let mut stack = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--manifest" => set_once_parsed(&mut manifest, arguments.next(), parse_manifest_path),
            "--stack" => set_once_parsed(&mut stack, arguments.next(), parse_setup_task_name),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let request = ManifestEnsureJobRequest {
        workspace: PathBuf::from(workspace.unwrap_or_else(|| usage())),
        manifest: manifest.unwrap_or_else(|| usage()),
        stack: stack.unwrap_or_else(|| usage()),
        deadline: Duration::from_millis(deadline_ms.unwrap_or_else(|| usage())),
        output_limit: output_limit.unwrap_or_else(|| usage()),
    };
    let state_dir = state_dir.unwrap_or_else(|| usage());
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| usage());
    let client = Client::for_state(state_dir).unwrap_or_else(|_| usage());
    match runtime.run(client.submit_manifest_ensure(request)) {
        Ok(job_id) if json_output => println!(
            "{}",
            json!({"action":"manifest_ensure","submitted":true,"job_id":job_id})
        ),
        Ok(job_id) => println!("manifest ensure submitted: {job_id}"),
        Err(_) => usage(),
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

/// Read-only census of Docker artifacts Bosn does not own.
///
/// This reports; it never deletes, and it never suggests a Docker command. A census that
/// cannot be read completely is reported as partial and is never a clean machine.
fn run_scan(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut ttl_seconds = None;
    let mut warn_bytes = None;
    let mut warn_objects = None;
    let mut json_output = false;
    let mut ack = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--ttl-seconds" => set_once_parsed(&mut ttl_seconds, arguments.next(), parse_ttl_seconds),
            "--warn-bytes" => set_once_parsed(&mut warn_bytes, arguments.next(), parse_ttl_seconds),
            "--warn-objects" => set_once_parsed(&mut warn_objects, arguments.next(), parse_ttl_seconds),
            "--ack" if !ack => {
                ack = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let config = census_config(ttl_seconds);
    let threshold = warning_threshold(warn_bytes, warn_objects);
    let (scan, our_registry) = scan_host(&state_dir, config);
    let census = &scan.census;
    let warning = bosn_core::warning(census, threshold);
    let acknowledged = bosn_core::acknowledgement_suppresses(
        bosn_service::unmanaged::read_acknowledgement(&state_dir),
        census,
        now_seconds(),
    );
    if ack {
        if let Some(warning) = &warning {
            let stored = bosn_core::Acknowledgement {
                at: now_seconds(),
                objects: warning.reclaimable_objects,
                bytes: warning.reclaimable_bytes,
            };
            match bosn_service::unmanaged::write_acknowledgement(&state_dir, stored) {
                Ok(()) => {
                    if !json_output {
                        eprintln!("scan: acknowledged {} objects", stored.objects);
                    }
                }
                Err(detail) => {
                    eprintln!("scan: could not record the acknowledgement: {detail}");
                    std::process::exit(1);
                }
            }
        }
        return;
    }
    if json_output {
        println!("{}", scan_json(scan, warning.as_ref(), acknowledged));
        return;
    }
    println!("scan");
    print_census(census);
    for detail in &scan.unreadable {
        eprintln!("scan: partial: {detail}");
    }
    // A partial census is never a clean machine, so it warns regardless of size.
    if let Some(warning) = warning {
        if !acknowledged {
            print_warning(&warning);
        }
    }
    let _ = our_registry;
}

/// The census configuration, with the documented default age gate.
fn census_config(ttl_seconds: Option<f64>) -> bosn_core::CensusConfig {
    bosn_core::CensusConfig {
        ttl_seconds: ttl_seconds.unwrap_or(bosn_core::DEFAULT_TTL_SECONDS),
    }
}

fn warning_threshold(bytes: Option<f64>, objects: Option<f64>) -> bosn_core::WarningThreshold {
    let default = bosn_core::WarningThreshold::default();
    bosn_core::WarningThreshold {
        bytes: bytes.map_or(default.bytes, |value| value as i128),
        objects: objects.map_or(default.objects, |value| value as u64),
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// Run one census pass against this host, reading only this registry's UUID from state.
///
/// The registry is opened read-only and only for its identity. A missing or unreadable
/// registry leaves the UUID unknown, which classifies every complete label set as foreign:
/// protective, never exposing.
fn scan_host(
    state_dir: &Path,
    config: bosn_core::CensusConfig,
) -> (bosn_service::unmanaged::UnmanagedCensus, Option<String>) {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());
    let engine = DockerEngine::docker();
    let scan = bosn_service::unmanaged::unmanaged_census(&engine, our_registry.as_deref(), config);
    (scan, our_registry)
}

fn class_tier(tier: bosn_core::Tier) -> &'static str {
    match tier {
        bosn_core::Tier::Reclaimable => "reclaimable",
        bosn_core::Tier::Review => "review",
    }
}

fn print_census(census: &bosn_core::Census) {
    if census.classes.is_empty() {
        println!("nothing unowned observed");
    }
    for summary in &census.classes {
        println!(
            "{:<20} {:<12} {:>6} objects  {}  eligible {}",
            summary.class.as_str(),
            class_tier(summary.tier),
            summary.objects,
            bosn_service::unmanaged::human_bytes(summary.bytes),
            summary.eligible_objects,
        );
    }
    if census.reclaimable_objects > 0 {
        println!(
            "eligible: {} objects, {}",
            census.reclaimable_objects,
            bosn_service::unmanaged::human_bytes(census.reclaimable_bytes)
        );
    }
    for summary in &census.protected {
        println!(
            "protected: {:<26} {:>6} objects  {}",
            summary.reason.as_str(),
            summary.objects,
            bosn_service::unmanaged::human_bytes(summary.bytes)
        );
    }
}

fn scan_json(
    scan: bosn_service::unmanaged::UnmanagedCensus,
    warning: Option<&bosn_core::Warning>,
    acknowledged: bool,
) -> serde_json::Value {
    let census = &scan.census;
    let classes: Vec<_> = census
        .classes
        .iter()
        .map(|summary| {
            json!({
                "class": summary.class.as_str(),
                "tier": class_tier(summary.tier),
                "objects": summary.objects,
                "bytes": summary.bytes,
                "eligible_objects": summary.eligible_objects,
                "eligible_bytes": summary.eligible_bytes,
                "oldest_age_seconds": summary.oldest_age_seconds,
            })
        })
        .collect();
    let protected: Vec<_> = census
        .protected
        .iter()
        .map(|summary| {
            json!({
                "reason": summary.reason.as_str(),
                "objects": summary.objects,
                "bytes": summary.bytes,
            })
        })
        .collect();
    // JSON never carries caps or ANSI, and never a suggested Docker command.
    let foreign_reclaimable = warning.map_or(json!(null), |warning| {
        json!({
            "reclaimable_objects": warning.reclaimable_objects,
            "reclaimable_bytes": warning.reclaimable_bytes,
            "review_objects": warning.review_objects,
            "review_bytes": warning.review_bytes,
            "report_only_bytes": warning.report_only_bytes,
            "partial": warning.partial,
            "acknowledged": acknowledged,
        })
    });
    json!({
        "action": "scan",
        "reclaimable_objects": census.reclaimable_objects,
        "reclaimable_bytes": census.reclaimable_bytes,
        "bytes_approximate": census.bytes_approximate,
        "partial": census.partial,
        "classes": classes,
        "protected": protected,
        "foreign_reclaimable": foreign_reclaimable,
        "unreadable": scan.unreadable,
    })
}

/// Colour is opt-out by environment and by pipe, matching the existing problem-output
/// precedent. Caps survive; escape codes do not.
fn colour_enabled() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM").map(|term| term == "dumb").unwrap_or(false) {
        return false;
    }
    std::io::stderr().is_terminal()
}

/// The loud warning. Printed to stderr, at most once per invocation, and never in JSON.
///
/// The lines come from the shared formatter so the daemon's unattended pass and this
/// interactive surface say exactly the same thing.
fn print_warning(warning: &bosn_core::Warning) {
    let colour = colour_enabled();
    for (index, line) in bosn_service::unmanaged::warning_lines(warning).iter().enumerate() {
        // Only the headline shouts, and only it is coloured: an all-caps table is unreadable
        // and the shouting has to mean something.
        if colour && index == 0 {
            eprintln!("\x1b[33m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}

fn run_gc(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(verb) = arguments.next() else {
        usage();
    };
    if verb.as_os_str() == std::ffi::OsStr::new("--unmanaged") {
        return run_gc_unmanaged(arguments);
    }
    if verb.as_os_str() == std::ffi::OsStr::new("apply") {
        return run_gc_apply(arguments);
    }
    if verb.as_os_str() != std::ffi::OsStr::new("preview") {
        usage();
    }
    let (state_dir, workspace, after, limit, json_output) =
        parse_gc_preview_arguments(arguments).unwrap_or_else(|_| usage());
    let client = Client::for_state(state_dir).unwrap_or_else(|_| gc_failure(json_output));
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| gc_failure(json_output));
    match runtime.run(client.setup_gc_preview(workspace, after, limit)) {
        Ok(page) => {
            let candidates: Vec<_> = page.candidates.into_iter().map(|value| json!({"id": value.id, "name": value.name, "generation": value.generation, "token":value.token, "reason": value.reason})).collect();
            println!(
                "{}",
                json!({"action":"gc_preview", "preview_only":true, "next":page.next, "candidates":candidates, "counts":{"protected_not_retired":page.counts.protected_not_retired,"protected_ambiguous_use":page.counts.protected_ambiguous_use,"protected_lease":page.counts.protected_lease,"protected_session":page.counts.protected_session,"excluded_unmanaged":page.counts.excluded_unmanaged}})
            );
        }
        Err(_) => gc_failure(json_output),
    }
}
/// Explicit one-candidate destructive action. Both `--apply` and `--yes` are
/// required even though the subcommand is named apply, preventing accidental
/// shell/script invocation. The daemon revalidates ownership before Docker.
/// `bosn gc --unmanaged` — the human-triggered path for artifacts Bosn does not own.
///
/// The preview is read-only and lists exactly what a removal pass would take. It is a new
/// planning mode, not a widening of the token-bound owned-candidate apply: that protocol
/// proves a positive (this resource is ours and safe), while this proves a negative (nothing
/// proves this resource is ours, nothing uses it, and it is past its age gate).
fn run_gc_unmanaged(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut ttl_seconds = None;
    let mut include: Vec<String> = Vec::new();
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--ttl-seconds" => set_once_parsed(&mut ttl_seconds, arguments.next(), parse_ttl_seconds),
            "--include" => match arguments.next().and_then(|value| {
                value
                    .to_str()
                    .map(str::to_owned)
                    .filter(|value| !value.is_empty())
            }) {
                Some(value) => {
                    include.push(value);
                    Ok(())
                }
                None => Err(()),
            },
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    if apply {
        if !yes {
            eprintln!("bosn gc --unmanaged --apply: pass --yes to confirm the removal");
            std::process::exit(2);
        }
        // The daemon re-derives the census and the plan itself. The preview this process
        // could build is never trusted: it was taken against state that may have changed.
        let ttl = ttl_seconds.map_or(0u64, |value| value.max(0.0) as u64);
        let result = Client::for_state(state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.unmanaged_gc_apply(include.clone(), ttl, true))
                        .ok()
                })
        });
        let Some(summary) = result else {
            eprintln!("bosn gc --unmanaged --apply: daemon unavailable or request failed");
            std::process::exit(1);
        };
        if json_output {
            println!(
                "{}",
                json!({
                    "action": "gc_unmanaged_apply",
                    "planned": summary.planned,
                    "removed": summary.removed,
                    "removed_bytes": summary.removed_bytes,
                    "failed": summary.failed,
                    "failures": summary.failures,
                    "refused": summary.refused,
                })
            );
        } else {
            println!("gc --unmanaged --apply");
            println!("planned: {}", summary.planned);
            println!(
                "removed: {} objects, {}",
                summary.removed,
                bosn_service::unmanaged::human_bytes(summary.removed_bytes)
            );
            for failure in &summary.failures {
                println!("failed:  {failure}");
            }
        }
        if let Some(refused) = &summary.refused {
            eprintln!("gc --unmanaged: {refused}");
            std::process::exit(1);
        }
        return;
    }
    let config = census_config(ttl_seconds);
    let (scan, our_registry) = scan_host(&state_dir, config);
    if !scan.is_trustworthy() {
        // A plan is never built from a partial census.
        for detail in &scan.unreadable {
            eprintln!("gc --unmanaged: partial: {detail}");
        }
        eprintln!("gc --unmanaged: the census was incomplete, so nothing is proposed");
        std::process::exit(1);
    }
    let plan = bosn_core::plan(&scan.artifacts, our_registry.as_deref(), config, &include);
    if json_output {
        let candidates: Vec<_> = plan
            .candidates
            .iter()
            .map(|candidate| {
                json!({
                    "id": candidate.id,
                    "class": candidate.class.as_str(),
                    "bytes": candidate.bytes,
                })
            })
            .collect();
        let review: Vec<_> = plan
            .review
            .iter()
            .map(|candidate| {
                json!({
                    "id": candidate.id,
                    "class": candidate.class.as_str(),
                    "bytes": candidate.bytes,
                })
            })
            .collect();
        let report_only: Vec<_> = plan
            .report_only
            .iter()
            .map(|summary| {
                json!({
                    "class": summary.class.as_str(),
                    "objects": summary.eligible_objects,
                    "bytes": summary.eligible_bytes,
                    "reason": "no per-object removal",
                })
            })
            .collect();
        println!(
            "{}",
            json!({
                "action": "gc_unmanaged_preview",
                "preview_only": true,
                "apply_available": true,
                "bytes": plan.bytes,
                "candidates": candidates,
                "review": review,
                "report_only": report_only,
            })
        );
        return;
    }
    println!("gc --unmanaged (preview)");
    if plan.candidates.is_empty() {
        println!("nothing eligible for removal");
    }
    for candidate in &plan.candidates {
        println!(
            "{:<18} {:>10}  {}",
            candidate.class.as_str(),
            bosn_service::unmanaged::human_bytes(candidate.bytes),
            candidate.id
        );
    }
    if !plan.candidates.is_empty() {
        println!(
            "would remove {} objects, {}",
            plan.candidates.len(),
            bosn_service::unmanaged::human_bytes(plan.bytes)
        );
    }
    for summary in &plan.report_only {
        println!(
            "report only: {:<18} {} objects, {} — no per-object removal",
            summary.class.as_str(),
            summary.eligible_objects,
            bosn_service::unmanaged::human_bytes(summary.eligible_bytes)
        );
    }
    if !plan.review.is_empty() {
        println!(
            "review: {} objects await judgment; opt one in with --include <id>",
            plan.review.len()
        );
    }
}

fn run_gc_apply(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut token = None;
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--candidate" => set_once_parsed(&mut token, arguments.next(), |value| {
                value.to_str().map(str::to_owned).ok_or(())
            }),
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let (Some(state_dir), Some(workspace), Some(token)) = (state_dir, workspace, token) else {
        usage();
    };
    if !apply || !yes {
        usage();
    }
    let result = Client::for_state(state_dir).ok().and_then(|client| {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| {
                runtime
                    .run(client.setup_gc_apply(workspace, &token, true))
                    .ok()
            })
    });
    match result {
        Some(result) => println!(
            "{}",
            json!({"action":"gc_apply","removed":result.removed,"reconciled_missing":result.reconciled_missing})
        ),
        None => gc_failure(json_output),
    }
}
fn parse_gc_preview_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, PathBuf, u64, u32, bool), ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut after = None;
    let mut limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--after" => set_once_parsed(&mut after, arguments.next(), parse_u64),
            "--limit" => set_once_parsed(&mut limit, arguments.next(), parse_registry_limit),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok((
        state_dir.ok_or(())?,
        workspace.ok_or(())?,
        after.unwrap_or(0),
        limit.unwrap_or(64),
        json,
    ))
}
fn gc_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action":"gc_preview","error":"daemon unavailable or request failed"})
        );
    } else {
        eprintln!("bosn gc preview: daemon unavailable or request failed");
    }
    std::process::exit(1)
}

/// Read the fixed, daemon-owned health report. Argument parsing happens before
/// any runtime/IPC work and deliberately exposes neither Docker controls nor
/// diagnostic output/deadline controls.
fn run_doctor(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let (state_dir, json_output) =
        parse_daemon_client_arguments(arguments).unwrap_or_else(|_| usage());
    let report = Client::for_state(&state_dir)
        .ok()
        .and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| runtime.run(client.doctor()).ok())
        })
        .unwrap_or_else(|| bosn_service::DoctorReport {
            daemon: "unavailable".into(),
            registry: "unavailable".into(),
            engine: "unavailable".into(),
            client_version: None,
            server_version: None,
        });
    let value = json!({
        "action": "doctor",
        "daemon": report.daemon,
        "registry": report.registry,
        "engine": report.engine,
        "client_version": report.client_version,
        "server_version": report.server_version,
    });
    if json_output {
        println!("{value}");
    } else {
        println!("doctor");
        for (key, value) in value.as_object().expect("literal object") {
            if key != "action" {
                println!("{key}: {value}");
            }
        }
    }
    // The warning rides along with doctor because doctor is the command a user runs when
    // something is wrong. #147's failure was silence: the pile grew for 45 hours while the
    // tool was being used. This is the surface that would have caught it.
    doctor_unmanaged_warning(&state_dir);
}

/// Print the unmanaged-artifact warning after a doctor report, if it applies.
///
/// An unreachable engine is reported as unavailable rather than as a warning: crying
/// "not known to be clean" on every machine without Docker would make the loud warning the
/// noise it is meant not to be.
fn doctor_unmanaged_warning(state_dir: &Path) {
    let config = census_config(None);
    let (scan, _) = scan_host(state_dir, config);
    if scan.census.classes.is_empty() && !scan.unreadable.is_empty() {
        eprintln!("unmanaged artifacts: census unavailable (is the Docker engine reachable?)");
        return;
    }
    let Some(warning) = bosn_core::warning(&scan.census, warning_threshold(None, None)) else {
        return;
    };
    let acknowledged = bosn_core::acknowledgement_suppresses(
        bosn_service::unmanaged::read_acknowledgement(state_dir),
        &scan.census,
        now_seconds(),
    );
    if !acknowledged {
        print_warning(&warning);
    }
}

/// Bounded, read-only daemon diagnostics. These commands deliberately require
/// the already-running daemon: the CLI does not open, create, or migrate a
/// SQLite registry and therefore preserves the daemon's single-writer model.
fn run_registry(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(command) = arguments.next() else {
        usage();
    };
    if command.as_os_str() == std::ffi::OsStr::new("import-v4") {
        run_registry_import_v4(arguments);
        return;
    }
    if command.as_os_str() == std::ffi::OsStr::new("reconcile-v4") {
        run_registry_reconcile_v4(arguments);
        return;
    }
    let invocation = match command.to_string_lossy().as_ref() {
        "resources" => {
            parse_registry_arguments(arguments).map(|(state_dir, after, limit, json)| {
                RegistryInvocation::Resources {
                    state_dir,
                    after,
                    limit,
                    json,
                }
            })
        }
        "setup-ensure-events" => {
            parse_registry_arguments(arguments).map(|(state_dir, after, limit, json)| {
                RegistryInvocation::SetupEnsureEvents {
                    state_dir,
                    after,
                    limit,
                    json,
                }
            })
        }
        _ => Err(()),
    }
    .unwrap_or_else(|_| usage());
    let state_dir = invocation.state_dir();
    let json_output = invocation.json();
    let client = Client::for_state(state_dir)
        .unwrap_or_else(|_| registry_failure(invocation.action(), json_output));
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| registry_failure(invocation.action(), json_output));
    match invocation {
        RegistryInvocation::Resources { after, limit, .. } => {
            match runtime.run(client.registry_resources(after, limit)) {
                Ok(page) => print_registry_resources(page, json_output),
                Err(_) => registry_failure("resources", json_output),
            }
        }
        RegistryInvocation::SetupEnsureEvents { after, limit, .. } => {
            match runtime.run(client.setup_ensure_events(after, limit)) {
                Ok(page) => print_setup_ensure_events(page, json_output),
                Err(_) => registry_failure("setup-ensure-events", json_output),
            }
        }
    }
}

/// Explicit, offline-only Python-v4 registry cutover.  This is deliberately
/// not an RPC: a normal daemon must not be running while its destination
/// database is being published, and the importer keeps the old source intact
/// for reconciliation rather than attempting an engine adoption by name.
fn run_registry_import_v4(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut legacy_state_dir = None;
    let mut state_dir = None;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--legacy-state-dir" => {
                set_once_parsed(&mut legacy_state_dir, arguments.next(), parse_state_dir)
            }
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| registry_import_failure(json_output));
    }
    let (Some(legacy_state_dir), Some(state_dir)) = (legacy_state_dir, state_dir) else {
        registry_import_failure(json_output);
    };
    if !yes {
        registry_import_failure(json_output);
    }

    // Check before destination hardening, so an accidental same-directory
    // invocation cannot even change source directory metadata.  Recheck after
    // creation as a defense against aliases which only resolve once the
    // destination exists.
    let legacy_identity = fs::path_identity(&legacy_state_dir)
        .ok()
        .flatten()
        .unwrap_or_else(|| registry_import_failure(json_output));
    if fs::path_identity(&state_dir).ok().flatten() == Some(legacy_identity) {
        registry_import_failure(json_output);
    }
    if !ipc::owner_private_directory(&legacy_state_dir)
        .ok()
        .unwrap_or(false)
    {
        registry_import_failure(json_output);
    }
    if ipc::ensure_owner_private_directory(&state_dir).is_err()
        || fs::path_identity(&state_dir).ok().flatten() == Some(legacy_identity)
    {
        registry_import_failure(json_output);
    }

    let source = legacy_state_dir.join("registry.sqlite3");
    let destination = state_dir.join("registry.sqlite3");
    match bosn_registry::import_python_v4(&legacy_state_dir, &source, &destination) {
        Ok(report) => {
            let counts = report
                .table_counts
                .into_iter()
                .map(|(key, value)| (key, json!(value)))
                .collect::<serde_json::Map<String, serde_json::Value>>();
            println!(
                "{}",
                json!({
                    "action": "registry_import_v4",
                    "registry_id": report.registry_id,
                    "reconciliation_required": report.reconciliation_required,
                    "table_counts": counts,
                    "source_preserved": true,
                })
            );
        }
        Err(_) => registry_import_failure(json_output),
    }
}

fn registry_import_failure(json_output: bool) -> ! {
    if json_output {
        println!(
            "{}",
            json!({"action":"registry_import_v4","error":"cutover refused"})
        );
    } else {
        eprintln!("bosn registry import-v4: cutover refused");
    }
    std::process::exit(1)
}

/// Offline bridge completion. The imported gate prevents daemon startup and
/// normal writers; this command owns the database-inode writer lock, performs
/// fixed exact-name inspection, and never sends Docker a lifecycle verb.
fn run_registry_reconcile_v4(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(verb) = arguments.next() else {
        usage();
    };
    let mut state_dir = None;
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| registry_reconcile_failure(json_output));
    }
    let Some(state_dir) = state_dir else {
        registry_reconcile_failure(json_output);
    };
    let is_apply = verb.as_os_str() == std::ffi::OsStr::new("apply");
    if !(verb.as_os_str() == std::ffi::OsStr::new("preview") || is_apply)
        || (is_apply && (!apply || !yes))
        || (!is_apply && (apply || yes))
    {
        registry_reconcile_failure(json_output);
    }
    let executor = OfflinePythonV4Docker {
        engine: DockerEngine::docker(),
    };
    let path = state_dir.join("registry.sqlite3");
    let result = if is_apply {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs_f64())
            .ok_or(());
        at.and_then(|at| apply_python_v4_reconciliation(&path, &executor, at).map_err(|_| ()))
    } else {
        preview_python_v4_reconciliation(&path, &executor).map_err(|_| ())
    };
    match result {
        Ok(report) => {
            println!(
                "{}",
                json!({
                    "action": if is_apply { "registry_reconcile_v4_apply" } else { "registry_reconcile_v4_preview" },
                    "preview_only": !is_apply,
                    "verified": report.verified,
                    "refusals": report.refusals,
                    "reconciliation_cleared": is_apply && report.ready(),
                })
            );
            if is_apply && !report.ready() {
                std::process::exit(1);
            }
        }
        Err(()) => registry_reconcile_failure(json_output),
    }
}

fn registry_reconcile_failure(json_output: bool) -> ! {
    if json_output {
        println!(
            "{}",
            json!({"action":"registry_reconcile_v4","error":"reconciliation refused"})
        );
    } else {
        eprintln!("bosn registry reconcile-v4: reconciliation refused");
    }
    std::process::exit(1)
}

struct OfflinePythonV4Docker {
    engine: DockerEngine,
}
impl PythonV4ReconcileExecutor for OfflinePythonV4Docker {
    fn inspect(
        &self,
        kind: ResourceKind,
        name: &str,
    ) -> Result<Option<PythonV4ObservedResource>, String> {
        let format = match kind {
            ResourceKind::Container => "{{.Id}}\t{{.Name}}\t{{json .Config.Labels}}",
            ResourceKind::Volume => "{{.Name}}\t{{.Name}}\t{{json .Labels}}",
            ResourceKind::Image => "{{.Id}}\t{{.Id}}\t{{json .Config.Labels}}",
            ResourceKind::Network => "{{.Id}}\t{{.Name}}\t{{json .Labels}}",
            ResourceKind::Builder => return Ok(None),
        };
        let object = match kind {
            ResourceKind::Image => "image",
            ResourceKind::Volume => "volume",
            ResourceKind::Network => "network",
            _ => "container",
        };
        let result = self
            .engine
            .with_args([object, "inspect", "--format", format, name])
            .capture(RunOptions::bounded(Duration::from_secs(3), 16 * 1024))
            .map_err(|_| "inspect failed".to_owned())?;
        if result.exit_code == 1 {
            return Ok(None);
        }
        if !result.ok() {
            return Err("inspect failed".into());
        }
        let text = std::str::from_utf8(&result.stdout).map_err(|_| "invalid inspect".to_owned())?;
        let fields: Vec<_> = text
            .trim_end_matches(['\r', '\n'])
            .splitn(3, '\t')
            .collect();
        if fields.len() != 3 || fields[0].is_empty() || fields[1].is_empty() {
            return Err("invalid inspect".into());
        }
        let labels =
            serde_json::from_str(fields[2]).map_err(|_| "invalid inspect labels".to_owned())?;
        Ok(Some(PythonV4ObservedResource {
            engine_id: fields[0].into(),
            name: fields[1].into(),
            labels,
        }))
    }
}

enum RegistryInvocation {
    Resources {
        state_dir: PathBuf,
        after: u64,
        limit: u32,
        json: bool,
    },
    SetupEnsureEvents {
        state_dir: PathBuf,
        after: u64,
        limit: u32,
        json: bool,
    },
}
impl RegistryInvocation {
    fn state_dir(&self) -> &std::path::Path {
        match self {
            Self::Resources { state_dir, .. } | Self::SetupEnsureEvents { state_dir, .. } => {
                state_dir
            }
        }
    }
    fn json(&self) -> bool {
        match self {
            Self::Resources { json, .. } | Self::SetupEnsureEvents { json, .. } => *json,
        }
    }
    fn action(&self) -> &'static str {
        match self {
            Self::Resources { .. } => "resources",
            Self::SetupEnsureEvents { .. } => "setup-ensure-events",
        }
    }
}

fn parse_registry_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, u64, u32, bool), ()> {
    let mut state_dir = None;
    let mut after = None;
    let mut limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--after" => set_once_parsed(&mut after, arguments.next(), parse_u64),
            "--limit" => set_once_parsed(&mut limit, arguments.next(), parse_registry_limit),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok((
        state_dir.ok_or(())?,
        after.unwrap_or(0),
        limit.unwrap_or(64),
        json,
    ))
}

fn parse_registry_limit(value: std::ffi::OsString) -> Result<u32, ()> {
    let value = parse_u64(value)?;
    (1..=u64::from(bosn_service::MAX_REGISTRY_DIAGNOSTIC_PAGE))
        .contains(&value)
        .then_some(value as u32)
        .ok_or(())
}

fn registry_failure(action: &str, json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": format!("registry_{action}"), "error": "daemon unavailable or request failed"})
        );
    } else {
        eprintln!("bosn registry {action}: daemon unavailable or request failed");
    }
    std::process::exit(1)
}

fn print_registry_resources(page: bosn_service::RegistryResourcePage, _json_output: bool) {
    let records: Vec<_> = page.records.into_iter().map(|record| json!({"id": record.id, "kind": record.kind, "name": record.name, "stack": record.stack, "generation": record.generation, "state": record.state, "retention": record.retention, "created_at": record.created_at, "last_used": record.last_used})).collect();
    let value = json!({"action": "registry_resources", "next": page.next, "records": records});
    println!("{value}");
}
fn print_setup_ensure_events(page: bosn_service::SetupEnsureEventPage, _json_output: bool) {
    let records: Vec<_> = page.records.into_iter().map(|record| json!({"cursor": record.cursor, "at": record.at, "kind": record.kind, "detail": record.detail})).collect();
    let value = json!({"action": "setup_ensure_events", "next": page.next, "records": records});
    println!("{value}");
}

/// Run the deliberately small, package-ready foreground daemon surface. It
/// does not fork, register an autostart entry, or make Docker calls. The
/// service itself owns state-directory hardening, registry-writer exclusion,
/// and authenticated local IPC.
/// Register or unregister the daemon with the platform's user service manager.
///
/// Writing the entry file is not the operation — registering it is. The Python
/// implementation wrote a LaunchAgent plist and returned, so a macOS user who asked for
/// autostart got nothing until their next login.
fn run_daemon_autostart(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(verb) = arguments.next() else {
        usage();
    };
    let mut state_dir = None;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let Some(platform) = bosn_service::autostart::Platform::current() else {
        eprintln!("bosn daemon autostart: this platform has no supported service manager");
        std::process::exit(2);
    };
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        eprintln!("bosn daemon autostart: HOME is not set");
        std::process::exit(2);
    };
    let runner = bosn_service::autostart::SystemRunner;
    let state_dir = state_dir.unwrap_or_else(bosn_service::mcp::default_state_dir);
    let action = verb.to_string_lossy().into_owned();
    let result = match action.as_str() {
        "enable" => std::env::current_exe()
            .map_err(|error| error.to_string())
            .and_then(|binary| {
                bosn_service::autostart::enable(&runner, platform, &home, &binary, &state_dir)
            }),
        "disable" => bosn_service::autostart::disable(&runner, platform, &home),
        "status" => Ok(bosn_service::autostart::status(platform, &home)),
        _ => usage(),
    };
    match result {
        Ok(status) => {
            if json_output {
                println!(
                    "{}",
                    json!({
                        "action": format!("daemon_autostart_{action}"),
                        "written": status.written,
                        "registered": status.registered,
                        "path": status.path.to_string_lossy(),
                    })
                );
            } else {
                println!("daemon autostart {action}");
                println!("entry:      {}", status.path.to_string_lossy());
                println!("written:    {}", status.written);
                println!("registered: {}", status.registered);
            }
        }
        Err(detail) => {
            eprintln!("bosn daemon autostart {action}: {detail}");
            std::process::exit(1);
        }
    }
}

fn run_daemon(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(command) = arguments.next() else {
        usage();
    };
    // Autostart is not a daemon invocation: it never contacts a running daemon, and it
    // returns rather than entering the runtime below.
    if command.to_string_lossy() == "autostart" {
        run_daemon_autostart(arguments);
        return;
    }
    let invocation = match command.to_string_lossy().as_ref() {
        "serve" => parse_daemon_serve_arguments(arguments)
            .map(|state_dir| DaemonInvocation::Serve { state_dir }),
        "status" => parse_daemon_client_arguments(arguments)
            .map(|(state_dir, json)| DaemonInvocation::Status { state_dir, json }),
        "stop" => parse_daemon_client_arguments(arguments)
            .map(|(state_dir, json)| DaemonInvocation::Stop { state_dir, json }),
        _ => Err(()),
    };
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };

    let runtime = match RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => daemon_failure(invocation.action(), invocation.json()),
    };
    match invocation {
        DaemonInvocation::Serve { state_dir } => {
            if runtime
                .run(bosn_service::Service::new(state_dir).serve())
                .is_err()
            {
                daemon_failure("serve", false);
            }
            println!("daemon stopped");
        }
        DaemonInvocation::Status { state_dir, json } => {
            let status = match Client::for_state(&state_dir)
                .and_then(|client| runtime.run(client.status()))
            {
                Ok(status) => status,
                Err(_) => daemon_failure("status", json),
            };
            print_daemon_status(&status, json);
        }
        DaemonInvocation::Stop { state_dir, json } => {
            if Client::for_state(&state_dir)
                .and_then(|client| runtime.run(client.shutdown()))
                .is_err()
            {
                daemon_failure("stop", json);
            }
            if json {
                println!("{}", json!({"action": "daemon_stop", "stopped": true}));
            } else {
                println!("daemon stopped");
            }
        }
    }
}

enum DaemonInvocation {
    Serve { state_dir: PathBuf },
    Status { state_dir: PathBuf, json: bool },
    Stop { state_dir: PathBuf, json: bool },
}

impl DaemonInvocation {
    fn action(&self) -> &'static str {
        match self {
            Self::Serve { .. } => "serve",
            Self::Status { .. } => "status",
            Self::Stop { .. } => "stop",
        }
    }

    fn json(&self) -> bool {
        match self {
            Self::Serve { .. } => false,
            Self::Status { json, .. } | Self::Stop { json, .. } => *json,
        }
    }
}

/// Parse every daemon argument before constructing a runtime, asking the
/// client to resolve an endpoint, or allowing `serve` to create state.
fn parse_daemon_serve_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PathBuf, ()> {
    let mut state_dir = None;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            _ => Err(()),
        }?;
    }
    state_dir.ok_or(())
}

fn parse_daemon_client_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, bool), ()> {
    let mut state_dir = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok((state_dir.ok_or(())?, json))
}

fn daemon_failure(action: &str, json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": format!("daemon_{action}"), "error": "request failed"})
        );
    } else {
        eprintln!("bosn daemon {action}: request failed");
    }
    std::process::exit(1)
}

fn print_daemon_status(status: &bosn_service::Status, json: bool) {
    if json {
        println!(
            "{}",
            json!({
                "action": "daemon_status",
                "daemon": "online",
                "registry_id": status.registry_id,
                "schema_version": status.schema_version,
                "resources": status.resources,
                "leases": status.leases,
                "sessions": status.sessions,
                "reconciliation_required": status.reconciliation_required,
            })
        );
    } else {
        println!("daemon status");
        println!("daemon: online");
        println!("registry_id: {}", status.registry_id);
        println!("schema_version: {}", status.schema_version);
        println!("resources: {}", status.resources);
        println!("leases: {}", status.leases);
        println!("sessions: {}", status.sessions);
        println!(
            "reconciliation_required: {}",
            status.reconciliation_required
        );
    }
}

fn run_mcp(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let state_dir = match arguments.next() {
        None => bosn_service::mcp::default_state_dir(),
        Some(flag) if flag == "--state-dir" => match arguments.next() {
            Some(path) if arguments.next().is_none() => PathBuf::from(path),
            _ => usage(),
        },
        _ => usage(),
    };
    if let Err(error) = bosn_service::mcp::serve_stdio(state_dir) {
        eprintln!("bosn mcp: {error}");
        std::process::exit(1);
    }
}

fn run_setup(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    match arguments.next().as_deref() {
        Some(command) if command == "plan" => run_setup_plan(arguments),
        Some(command) if command == "prepare" => run_setup_prepare(arguments),
        Some(command) if command == "task" => run_setup_task(arguments),
        Some(command) if command == "app-task" => run_setup_app_task(arguments),
        Some(command) if command == "ensure" => run_setup_ensure(arguments),
        Some(command) if command == "reconcile" => run_setup_reconcile(arguments),
        Some(command) if command == "adopt" => run_setup_adopt(arguments),
        Some(command) if command == "stop-retired" => run_setup_stop_retired(arguments),
        Some(command) if command == "done" => run_setup_done(arguments),
        _ => usage(),
    }
}

fn run_setup_reconcile(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    match arguments.next().as_deref() {
        Some(command) if command == "preview" => {}
        Some(command) if command == "repair-missing" => {
            run_setup_reconcile_repair_missing(arguments);
            return;
        }
        _ => usage(),
    }
    let (state_dir, workspace, after, limit, json_output) =
        parse_gc_preview_arguments(arguments).unwrap_or_else(|_| usage());
    let result = Client::for_state(state_dir).ok().and_then(|client| {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| {
                runtime
                    .run(client.setup_reconcile_preview(workspace, after, limit))
                    .ok()
            })
    });
    match result {
        Some(page) => println!(
            "{}",
            json!({"action":"setup_reconcile_preview","preview_only":true,"next":page.next,"records":page.records.into_iter().map(|v| json!({"id":v.id,"name":v.name,"generation":v.generation,"drift":v.drift})).collect::<Vec<_>>() })
        ),
        None => {
            if json_output {
                println!(
                    "{}",
                    json!({"action":"setup_reconcile_preview","error":"daemon unavailable or request failed"})
                );
            } else {
                eprintln!("bosn setup reconcile preview: daemon unavailable or request failed");
            }
            std::process::exit(1);
        }
    }
}

/// Confirmation-gated registry repair for one opaque missing-drift preview
/// token. The CLI never accepts a Docker identifier or lifecycle control.
fn run_setup_reconcile_repair_missing(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut token = None;
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--candidate" => set_once_parsed(&mut token, arguments.next(), |value| {
                value.to_str().map(str::to_owned).ok_or(())
            }),
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let (Some(state_dir), Some(workspace), Some(token)) = (state_dir, workspace, token) else {
        usage();
    };
    if !apply || !yes {
        usage();
    }
    let result = Client::for_state(&state_dir).ok().and_then(|client| {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| {
                runtime
                    .run(client.setup_reconcile_repair_missing(workspace, &token, true))
                    .ok()
            })
    });
    match result {
        Some(result) => println!(
            "{}",
            json!({"action":"setup_reconcile_repair_missing","repaired":result.repaired,"already_repaired":result.already_repaired})
        ),
        None => {
            if json_output {
                println!(
                    "{}",
                    json!({"action":"setup_reconcile_repair_missing","error":"daemon unavailable or request failed"})
                );
            } else {
                eprintln!(
                    "bosn setup reconcile repair-missing: daemon unavailable or request failed"
                );
            }
            std::process::exit(1);
        }
    }
}

/// Stop one preview-derived retired generation while retaining its registry
/// record for the separate GC apply action. No Docker identifier or engine
/// controls are accepted by this CLI.
fn run_setup_stop_retired(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut token = None;
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--candidate" => set_once_parsed(&mut token, arguments.next(), |value| {
                value.to_str().map(str::to_owned).ok_or(())
            }),
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let (Some(state_dir), Some(workspace), Some(token)) = (state_dir, workspace, token) else {
        usage();
    };
    if !apply || !yes {
        usage();
    }
    let result = Client::for_state(&state_dir).ok().and_then(|client| {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| {
                runtime
                    .run(client.setup_stop_retired(workspace, &token, true))
                    .ok()
            })
    });
    match result {
        Some(result) => println!(
            "{}",
            json!({"action":"setup_stop_retired","stopped":result.stopped,"already_stopped":result.already_stopped})
        ),
        None => {
            if json_output {
                println!(
                    "{}",
                    json!({"action":"setup_stop_retired","error":"daemon unavailable or request failed"})
                );
            } else {
                eprintln!("bosn setup stop-retired: daemon unavailable or request failed");
            }
            std::process::exit(1);
        }
    }
}

/// Confirmed recovery of lost registry ownership for one already-existing
/// managed app. The daemon re-derives all Docker identity; CLI never accepts it.
fn run_setup_adopt(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut values = Vec::new();
    let mut yes = false;
    for arg in arguments {
        if arg == "--yes" && !yes {
            yes = true;
        } else {
            values.push(arg);
        }
    }
    if !yes {
        usage();
    }
    let invocation = match parse_ensure_arguments(values.into_iter()) {
        Ok(value) => value,
        Err(_) => usage(),
    };
    let request = bosn_service::SetupAdoptRequest {
        workspace: invocation.request.workspace,
        config: invocation.request.config,
        policy: invocation.request.policy,
        deadline: invocation.request.deadline,
        output_limit: invocation.request.output_limit,
        confirm: true,
    };
    let result = Client::for_state(&invocation.state_dir)
        .ok()
        .and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| runtime.run(client.setup_adopt(request)).ok())
        });
    match result {
        Some(value) => println!(
            "{}",
            json!({"action":"setup_adopt","adopted":value.adopted})
        ),
        None => setup_ensure_failure(invocation.json),
    }
}

fn run_setup_plan(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_plan_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bosn setup plan: runtime initialization: {error}");
            std::process::exit(1);
        }
    };
    let plan = match runtime.run(plan_setup(invocation.request)) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("bosn setup plan: {error}");
            std::process::exit(1);
        }
    };
    if invocation.json {
        print_json(&plan);
    } else {
        print_text(&plan);
    }
}

/// Submit a bounded, semantic setup-image preparation job to an already-running
/// daemon. This command intentionally neither launches a daemon nor talks to
/// Docker: submission is the only synchronous effect.
fn run_setup_prepare(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_prepare_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let client = match Client::for_state(&invocation.state_dir) {
        Ok(client) => client,
        Err(_) => setup_prepare_failure(),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => setup_prepare_failure(),
    };
    let job_id = match runtime.run(client.submit_setup_prepare(invocation.request)) {
        Ok(job_id) => job_id,
        Err(_) => setup_prepare_failure(),
    };
    if invocation.json {
        println!(
            "{}",
            json!({"action": "setup_prepare", "submitted": true, "job_id": job_id})
        );
    } else {
        println!("setup prepare submitted");
        println!("job_id: {job_id}");
    }
}

/// Submit one declared setup task to an already-running daemon. This command
/// accepts a task name, not a task command: plan, image preparation, and task
/// execution remain owned by the daemon and validated setup document.
fn run_setup_task(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_task_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let client = match Client::for_state(&invocation.state_dir) {
        Ok(client) => client,
        Err(_) => setup_task_failure(invocation.json),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => setup_task_failure(invocation.json),
    };
    let job_id = match runtime.run(client.submit_setup_task(invocation.request)) {
        Ok(job_id) => job_id,
        Err(_) => setup_task_failure(invocation.json),
    };
    if invocation.json {
        println!(
            "{}",
            json!({"action": "setup_task", "submitted": true, "job_id": job_id})
        );
    } else {
        println!("setup task submitted");
        println!("job_id: {job_id}");
    }
}

/// Submit one declared task for the already ensured setup app. Parsing is
/// intentionally identical to `setup task`: the extra semantic is selected
/// only by this verb, never by a caller-controlled container or command flag.
fn run_setup_app_task(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_task_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let request = bosn_service::SetupAppTaskJobRequest {
        workspace: invocation.request.workspace,
        config: invocation.request.config,
        policy: invocation.request.policy,
        task_name: invocation.request.task_name,
        deadline: invocation.request.deadline,
        output_limit: invocation.request.output_limit,
    };
    let client = match Client::for_state(&invocation.state_dir) {
        Ok(client) => client,
        Err(_) => setup_task_failure(invocation.json),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => setup_task_failure(invocation.json),
    };
    let job_id = match runtime.run(client.submit_setup_app_task(request)) {
        Ok(job_id) => job_id,
        Err(_) => setup_task_failure(invocation.json),
    };
    if invocation.json {
        println!(
            "{}",
            json!({"action": "setup_app_task", "submitted": true, "job_id": job_id})
        );
    } else {
        println!("setup app task submitted");
        println!("job_id: {job_id}");
    }
}

/// Submit one ownership-safe setup application ensure to an already-running
/// daemon. This is deliberately only a semantic submission: it cannot start a
/// daemon or invoke Docker itself, and the daemon derives every engine choice
/// from the validated setup document and its prepared-image receipt.
fn run_setup_ensure(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_ensure_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let client = match Client::for_state(&invocation.state_dir) {
        Ok(client) => client,
        Err(_) => setup_ensure_failure(invocation.json),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => setup_ensure_failure(invocation.json),
    };
    let job_id = match runtime.run(client.submit_setup_ensure(invocation.request)) {
        Ok(job_id) => job_id,
        Err(_) => setup_ensure_failure(invocation.json),
    };
    if invocation.json {
        println!(
            "{}",
            json!({"action": "setup_ensure", "submitted": true, "job_id": job_id})
        );
    } else {
        println!("setup ensure submitted");
        println!("job_id: {job_id}");
    }
}

/// Explicitly finish one workspace's setup accounting. This is a daemon-only
/// registry transition: it never invokes Docker or removes resources.
fn run_setup_done(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut workspace = None;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json_output => {
                json_output = true;
                Ok(())
            }
            _ => Err(()),
        }
        .unwrap_or_else(|_| usage());
    }
    let (Some(state_dir), Some(workspace)) = (state_dir, workspace) else {
        usage();
    };
    if !yes {
        usage();
    }
    let result = Client::for_state(&state_dir).ok().and_then(|client| {
        RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .ok()
            .and_then(|runtime| runtime.run(client.setup_done(workspace, true)).ok())
    });
    match result {
        Some(result) => println!(
            "{}",
            json!({"action":"setup_done","uses_completed":result.uses_completed,"resources_completed":result.resources_completed})
        ),
        None => {
            if json_output {
                println!(
                    "{}",
                    json!({"action":"setup_done","error":"daemon unavailable or request failed"})
                );
            } else {
                eprintln!("bosn setup done: daemon unavailable or request failed");
            }
            std::process::exit(1);
        }
    }
}

/// Observe or cancel an existing daemon-owned job.  These commands neither
/// start the daemon nor invoke Docker; the authenticated client request is
/// their only side effect.
fn run_job(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let Some(command) = arguments.next() else {
        usage();
    };
    let invocation = match command.to_string_lossy().as_ref() {
        "status" => parse_job_status_arguments(arguments),
        "logs" => parse_job_logs_arguments(arguments),
        "cancel" => parse_job_cancel_arguments(arguments),
        _ => Err(()),
    };
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    let client = match Client::for_state(invocation.state_dir()) {
        Ok(client) => client,
        Err(_) => job_failure(invocation.action(), invocation.json()),
    };
    let runtime = match RuntimeBuilder::current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(_) => job_failure(invocation.action(), invocation.json()),
    };
    match invocation {
        JobInvocation::Status { job_id, json, .. } => {
            let status = match runtime.run(client.job_status(job_id)) {
                Ok(status) => status,
                Err(_) => job_failure("status", json),
            };
            print_job_status(&status, json);
        }
        JobInvocation::Logs {
            job_id,
            after,
            limit,
            json,
            ..
        } => {
            let page = match runtime.run(client.job_logs(job_id, after, limit)) {
                Ok(page) => page,
                Err(_) => job_failure("logs", json),
            };
            print_job_logs(job_id, &page, json);
        }
        JobInvocation::Cancel { job_id, json, .. } => {
            if runtime.run(client.cancel_job(job_id)).is_err() {
                job_failure("cancel", json);
            }
            if json {
                println!(
                    "{}",
                    json!({"action": "job_cancel", "cancelled": true, "job_id": job_id})
                );
            } else {
                println!("job cancelled");
                println!("job_id: {job_id}");
            }
        }
    }
}

enum JobInvocation {
    Status {
        state_dir: PathBuf,
        job_id: u64,
        json: bool,
    },
    Logs {
        state_dir: PathBuf,
        job_id: u64,
        after: u64,
        limit: u32,
        json: bool,
    },
    Cancel {
        state_dir: PathBuf,
        job_id: u64,
        json: bool,
    },
}

impl JobInvocation {
    fn state_dir(&self) -> &std::path::Path {
        match self {
            Self::Status { state_dir, .. }
            | Self::Logs { state_dir, .. }
            | Self::Cancel { state_dir, .. } => state_dir,
        }
    }

    fn json(&self) -> bool {
        match self {
            Self::Status { json, .. } | Self::Logs { json, .. } | Self::Cancel { json, .. } => {
                *json
            }
        }
    }

    fn action(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status",
            Self::Logs { .. } => "logs",
            Self::Cancel { .. } => "cancel",
        }
    }
}

fn parse_job_status_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let (state_dir, job_id, json) = parse_job_base_arguments(arguments)?;
    Ok(JobInvocation::Status {
        state_dir,
        job_id,
        json,
    })
}

fn parse_job_cancel_arguments(
    arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let (state_dir, job_id, json) = parse_job_base_arguments(arguments)?;
    Ok(JobInvocation::Cancel {
        state_dir,
        job_id,
        json,
    })
}

fn parse_job_logs_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<JobInvocation, ()> {
    let mut state_dir = None;
    let mut job_id = None;
    let mut after = None;
    let mut limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--job-id" => set_once_parsed(&mut job_id, arguments.next(), parse_job_id),
            "--after" => set_once_parsed(&mut after, arguments.next(), parse_u64),
            "--limit" => set_once_parsed(&mut limit, arguments.next(), parse_job_log_limit),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(JobInvocation::Logs {
        state_dir: state_dir.ok_or(())?,
        job_id: job_id.ok_or(())?,
        after: after.unwrap_or(0),
        limit: limit.unwrap_or(DEFAULT_JOB_LOG_LIMIT),
        json,
    })
}

fn parse_job_base_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, u64, bool), ()> {
    let mut state_dir = None;
    let mut job_id = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--job-id" => set_once_parsed(&mut job_id, arguments.next(), parse_job_id),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok((state_dir.ok_or(())?, job_id.ok_or(())?, json))
}

fn parse_state_dir(value: std::ffi::OsString) -> Result<PathBuf, ()> {
    let path = PathBuf::from(value);
    (!path.as_os_str().is_empty()).then_some(path).ok_or(())
}

/// A non-negative, finite age gate in seconds. NaN and infinity are refused rather than
/// silently treated as "no gate", which would make everything eligible.
fn parse_ttl_seconds(value: std::ffi::OsString) -> Result<f64, ()> {
    let seconds: f64 = value.to_str().ok_or(())?.parse().map_err(|_| ())?;
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds).ok_or(())
}

fn parse_job_id(value: std::ffi::OsString) -> Result<u64, ()> {
    let id = parse_u64(value)?;
    (id > 0).then_some(id).ok_or(())
}

fn parse_job_log_limit(value: std::ffi::OsString) -> Result<u32, ()> {
    let limit = parse_u64(value)?;
    (1..=u64::from(MAX_JOB_LOG_LIMIT))
        .contains(&limit)
        .then_some(limit as u32)
        .ok_or(())
}

fn job_failure(action: &str, json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": format!("job_{action}"), "error": "request failed"})
        );
    } else {
        eprintln!("bosn job {action}: request failed");
    }
    std::process::exit(1)
}

fn print_job_status(status: &JobStatus, json: bool) {
    if json {
        println!(
            "{}",
            json!({
                "action": "job_status",
                "job": {"id": status.id, "state": status.state, "error": status.error},
            })
        );
    } else {
        println!("job status");
        println!("job_id: {}", status.id);
        println!("state: {}", status.state);
        if let Some(error) = &status.error {
            println!("error: {error}");
        }
    }
}

fn print_job_logs(job_id: u64, page: &JobLogPage, json: bool) {
    if json {
        let records: Vec<_> = page
            .records
            .iter()
            .map(|record| json!({"cursor": record.cursor, "line": record.line}))
            .collect();
        println!(
            "{}",
            json!({
                "action": "job_logs",
                "job_id": job_id,
                "retained_from": page.retained_from,
                "next": page.next,
                "gap": page.gap,
                "records": records,
            })
        );
    } else {
        println!("job logs");
        println!("job_id: {job_id}");
        println!("retained_from: {}", page.retained_from);
        println!("next: {}", page.next);
        println!("gap: {}", page.gap);
        for record in &page.records {
            println!("{}: {}", record.cursor, record.line);
        }
    }
}

fn setup_prepare_failure() -> ! {
    eprintln!("bosn setup prepare: submission failed");
    std::process::exit(1)
}

fn setup_task_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": "setup_task", "error": "request failed"})
        );
    } else {
        eprintln!("bosn setup task: submission failed");
    }
    std::process::exit(1)
}

fn setup_ensure_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action": "setup_ensure", "error": "request failed"})
        );
    } else {
        eprintln!("bosn setup ensure: submission failed");
    }
    std::process::exit(1)
}

struct PlanInvocation {
    request: SetupPlanRequest,
    json: bool,
}

struct PrepareInvocation {
    state_dir: PathBuf,
    request: SetupPrepareRequest,
    json: bool,
}

struct TaskInvocation {
    state_dir: PathBuf,
    request: SetupTaskJobRequest,
    json: bool,
}

struct EnsureInvocation {
    state_dir: PathBuf,
    request: SetupEnsureJobRequest,
    json: bool,
}

/// The native plan command requires every stateful choice to be written on the
/// command line.  This prevents an unnoticed cache read or ambient workspace
/// from becoming an implicit apply input.
fn parse_plan_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PlanInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut locator = None;
    let mut policy = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once(&mut state_dir, arguments.next()),
            "--workspace" => set_once(&mut workspace, arguments.next()),
            "--config" => set_once(&mut locator, arguments.next()),
            "--refresh" => set_once(&mut policy, Some(SetupAcquirePolicy::OnlineRefresh)),
            "--offline" => set_once(&mut policy, Some(SetupAcquirePolicy::OfflineCacheOnly)),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(PlanInvocation {
        request: SetupPlanRequest {
            state_dir: PathBuf::from(state_dir.ok_or(())?),
            workspace: PathBuf::from(workspace.ok_or(())?),
            locator: locator.ok_or(())?.into_string().map_err(|_| ())?,
            policy: policy.ok_or(())?,
        },
        json,
    })
}

/// Parse every preparation input before creating a runtime or opening daemon
/// IPC. The daemon repeats equivalent wire validation; this boundary keeps bad
/// local invocations from making any state or daemon contact at all.
fn parse_prepare_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<PrepareInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once(&mut state_dir, arguments.next()),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(PrepareInvocation {
        state_dir: PathBuf::from(state_dir.ok_or(())?),
        request: SetupPrepareRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}

/// Parse every task-submission input before constructing a runtime or opening
/// daemon IPC. In particular, the only executable selection is a setup
/// document task name; callers cannot provide a command, mounts, environment,
/// work directory, container, or state override through the request.
fn parse_task_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<TaskInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut task_name = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--task" => set_once_parsed(&mut task_name, arguments.next(), parse_setup_task_name),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(TaskInvocation {
        state_dir: state_dir.ok_or(())?,
        request: SetupTaskJobRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            task_name: task_name.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}

/// Parse every ensure-submission input before constructing a runtime or
/// resolving the local IPC endpoint. The request intentionally has no app
/// command, image, container, mount, environment, work-directory, network,
/// privilege, label, state override, task, or raw Docker arguments.
fn parse_ensure_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<EnsureInvocation, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut config = None;
    let mut policy = None;
    let mut deadline_ms = None;
    let mut output_limit = None;
    let mut json = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => {
                set_once_parsed(&mut workspace, arguments.next(), parse_setup_request_text)
            }
            "--config" => set_once_parsed(&mut config, arguments.next(), parse_setup_config),
            "--refresh" => set_once(&mut policy, Some(SetupPreparePolicy::Refresh)),
            "--offline" => set_once(&mut policy, Some(SetupPreparePolicy::Offline)),
            "--deadline-ms" => set_once_parsed(&mut deadline_ms, arguments.next(), |value| {
                let value = parse_u64(value)?;
                (1..=SETUP_PREPARE_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=SETUP_PREPARE_MAX_OUTPUT_LIMIT)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }
    Ok(EnsureInvocation {
        state_dir: state_dir.ok_or(())?,
        request: SetupEnsureJobRequest {
            workspace: PathBuf::from(workspace.ok_or(())?),
            config: config.ok_or(())?,
            policy: policy.ok_or(())?,
            deadline: Duration::from_millis(deadline_ms.ok_or(())?),
            output_limit: output_limit.ok_or(())?,
        },
        json,
    })
}

fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(value.ok_or(())?);
    Ok(())
}

fn set_once_parsed<T>(
    slot: &mut Option<T>,
    value: Option<std::ffi::OsString>,
    parse: impl FnOnce(std::ffi::OsString) -> Result<T, ()>,
) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(parse(value.ok_or(())?)?);
    Ok(())
}

fn parse_u64(value: std::ffi::OsString) -> Result<u64, ()> {
    value.into_string().map_err(|_| ())?.parse().map_err(|_| ())
}

fn parse_usize(value: std::ffi::OsString) -> Result<usize, ()> {
    value.into_string().map_err(|_| ())?.parse().map_err(|_| ())
}

fn parse_setup_request_text(value: std::ffi::OsString) -> Result<String, ()> {
    const MAX_TEXT: usize = 8 * 1024;
    let value = value.into_string().map_err(|_| ())?;
    if value.is_empty() || value.len() > MAX_TEXT || value.bytes().any(|byte| byte == 0) {
        return Err(());
    }
    Ok(value)
}

fn parse_setup_config(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    parse_setup_config_locator(&value).map_err(|_| ())?;
    Ok(value)
}

fn parse_manifest_path(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    (!value.starts_with('/')
        && !value.contains('\\')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".."))
    .then_some(value)
    .ok_or(())
}

fn parse_setup_task_name(value: std::ffi::OsString) -> Result<String, ()> {
    let value = parse_setup_request_text(value)?;
    if value.len() > 64
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || byte == b'_' || (byte == b'-' && index > 0)
        })
    {
        return Err(());
    }
    Ok(value)
}

fn source_kind_name(source_kind: SetupSourceKind) -> &'static str {
    match source_kind {
        SetupSourceKind::LocalFile => "local_file",
        SetupSourceKind::Https => "https",
    }
}

fn print_text(plan: &SetupPlan) {
    println!("setup plan (not applied)");
    println!("source_kind: {}", source_kind_name(plan.source_kind));
    println!("content_sha256: {}", plan.content_sha256);
    println!("schema_version: {}", plan.schema_version);
    println!("workspace: {}", plan.workspace_root.display());
    println!(
        "asset_root: {}",
        plan.asset_root
            .as_deref()
            .map_or_else(|| "(none)".into(), |path| path.display().to_string())
    );
    println!("tasks: {}", plan.task_names.join(", "));
    match &plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => println!("app_source: pinned_image {image}"),
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            println!(
                "app_source: inline_dockerfile {}",
                dockerfile_path.display()
            )
        }
    }
}

fn print_json(plan: &SetupPlan) {
    let app_source = match &plan.app_source {
        SetupPlanAppSource::PinnedImage { image } => {
            json!({"kind": "pinned_image", "image": image})
        }
        SetupPlanAppSource::InlineDockerfile { dockerfile_path } => {
            json!({"kind": "inline_dockerfile", "dockerfile_path": dockerfile_path})
        }
    };
    println!(
        "{}",
        json!({
            "action": "plan",
            "applied": false,
            "source_kind": source_kind_name(plan.source_kind),
            "content_sha256": plan.content_sha256,
            "schema_version": plan.schema_version,
            "workspace": plan.workspace_root,
            "asset_root": plan.asset_root,
            "task_names": plan.task_names,
            "app_source": app_source,
        })
    );
}

fn usage() -> ! {
    eprintln!("   or: bosn act plan --workspace WORKSPACE --workflow RELATIVE_YML --event pull_request|push|release --mode minimal|test|full --sha 40_HEX --act-version VERSION [--act-bin PATH] [--job ID] [--json]");
    eprintln!("   or: bosn act run|report (refuses until isolated Docker ownership is implemented)");
    eprintln!("usage: bosn mcp [--state-dir STATE_DIR]");
    eprintln!("   or: bosn daemon serve --state-dir STATE_DIR");
    eprintln!("   or: bosn daemon status --state-dir STATE_DIR [--json]");
    eprintln!("   or: bosn daemon stop --state-dir STATE_DIR [--json]");
    eprintln!("   or: bosn daemon autostart (enable|disable|status) [--state-dir STATE_DIR] [--json]");
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
        "   or: bosn manifest app-task --state-dir STATE_DIR --workspace WORKSPACE --manifest RELATIVE_TOML --stack NAME --task NAME --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    eprintln!(
        "   or: bosn manifest converge --state-dir STATE_DIR --workspace WORKSPACE --manifest RELATIVE_TOML --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
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
