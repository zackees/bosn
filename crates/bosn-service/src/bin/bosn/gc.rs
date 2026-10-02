//! `bosn gc`: setup, manifest-volume and unmanaged collection.

use super::*;

pub(crate) fn run_gc(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
pub(crate) fn run_gc_unmanaged(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut ttl_seconds = None;
    let mut include: Vec<String> = Vec::new();
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--ttl-seconds" => {
                set_once_parsed(&mut ttl_seconds, arguments.next(), parse_ttl_seconds)
            }
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

pub(crate) fn run_gc_apply(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
pub(crate) fn parse_gc_preview_arguments(
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
pub(crate) fn gc_failure(json: bool) -> ! {
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
