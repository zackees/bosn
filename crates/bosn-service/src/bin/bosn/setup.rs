//! `bosn setup`: plan, prepare, task, ensure, adopt, reconcile, stop-retired, done.

use super::*;

pub(crate) fn run_setup(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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

pub(crate) fn run_setup_reconcile(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    require_matching_daemon(&state_dir, "setup reconcile preview", json_output);
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
pub(crate) fn run_setup_reconcile_repair_missing(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) {
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
    require_matching_daemon(&state_dir, "setup reconcile repair-missing", json_output);
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
pub(crate) fn run_setup_stop_retired(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    require_matching_daemon(&state_dir, "setup stop-retired", json_output);
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
pub(crate) fn run_setup_adopt(arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    require_matching_daemon(&invocation.state_dir, "setup adopt", invocation.json);
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

pub(crate) fn run_setup_plan(arguments: impl Iterator<Item = std::ffi::OsString>) {
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
pub(crate) fn run_setup_prepare(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_prepare_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    require_matching_daemon(&invocation.state_dir, "setup prepare", invocation.json);
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
pub(crate) fn run_setup_task(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_task_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    require_matching_daemon(&invocation.state_dir, "setup task", invocation.json);
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
pub(crate) fn run_setup_app_task(arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    require_matching_daemon(&invocation.state_dir, "setup app-task", invocation.json);
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
pub(crate) fn run_setup_ensure(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let invocation = match parse_ensure_arguments(arguments) {
        Ok(invocation) => invocation,
        Err(()) => usage(),
    };
    require_matching_daemon(&invocation.state_dir, "setup ensure", invocation.json);
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
pub(crate) fn run_setup_done(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    require_matching_daemon(&state_dir, "setup done", json_output);
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
