//! Native Bosn command development entry point.
//!
//! Python packaging invokes the same `bosn-service::mcp::serve_stdio` function
//! through PyO3 today. This binary makes `cargo run -p bosn-service --bin bosn
//! -- mcp` an equivalent, package-ready route without a Python launcher.  Its
//! setup route is a separate human/JSON CLI and never shares MCP stdio.

use std::path::PathBuf;

use bosn_setup::{
    SetupAcquirePolicy, SetupPlan, SetupPlanAppSource, SetupPlanRequest, SetupSourceKind,
    plan_setup,
};
use kernal_api::async_engine::RuntimeBuilder;
use serde_json::json;

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
        Some(command) if command == "plan" => {}
        _ => usage(),
    }
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

struct PlanInvocation {
    request: SetupPlanRequest,
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

fn set_once<T>(slot: &mut Option<T>, value: Option<T>) -> Result<(), ()> {
    if slot.is_some() {
        return Err(());
    }
    *slot = Some(value.ok_or(())?);
    Ok(())
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
    std::process::exit(2)
}
