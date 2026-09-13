//! Native Bosn command development entry point.
//!
//! Python packaging invokes the same `bosn-service::mcp::serve_stdio` function
//! through PyO3 today. This binary makes `cargo run -p bosn-service --bin bosn
//! -- mcp` an equivalent, package-ready route without a Python launcher.  Its
//! setup route is a separate human/JSON CLI and never shares MCP stdio.

use std::path::PathBuf;
use std::time::Duration;

use bosn_core::parse_setup_config_locator;
use bosn_service::{Client, SetupPreparePolicy, SetupPrepareRequest};
use bosn_setup::{
    SetupAcquirePolicy, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupSourceKind,
    plan_setup,
};
use kernal_api::async_engine::RuntimeBuilder;
use serde_json::json;

const SETUP_PREPARE_MAX_DEADLINE_MS: u64 = 5 * 60 * 1_000;
const SETUP_PREPARE_MAX_OUTPUT_LIMIT: usize = 8 * 1024 * 1024;

fn main() {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let Some(command) = arguments.next() else {
        usage();
    };
    match command.to_string_lossy().as_ref() {
        "mcp" => run_mcp(arguments),
        "setup" => run_setup(arguments),
        _ => usage(),
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
        _ => usage(),
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

fn setup_prepare_failure() -> ! {
    eprintln!("bosn setup prepare: submission failed");
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
    eprintln!("usage: bosn mcp [--state-dir STATE_DIR]");
    eprintln!(
        "   or: bosn setup plan --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) [--json]"
    );
    eprintln!(
        "   or: bosn setup prepare --state-dir STATE_DIR --workspace WORKSPACE --config LOCATOR (--refresh | --offline) --deadline-ms 1..=300000 --output-limit 1..=8388608 [--json]"
    );
    std::process::exit(2)
}
