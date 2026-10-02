//! `bosn manifest`: ensure, converge, app-task, volume GC and release.

use super::*;

pub(crate) fn run_manifest(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    match arguments.next().as_deref() {
        Some(command) if command == "ensure" => run_manifest_ensure(arguments),
        Some(command) if command == "converge" => run_manifest_converge(arguments),
        Some(command) if command == "app-task" => run_manifest_app_task(arguments),
        Some(command) if command == "volume-gc" => run_manifest_volume_gc(arguments),
        Some(command) if command == "volume-release" => run_manifest_volume_release(arguments),
        _ => usage(),
    }
}

pub(crate) fn run_manifest_volume_release(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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

pub(crate) fn run_manifest_volume_gc(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
pub(crate) fn run_manifest_converge(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
                (1..=MANIFEST_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=MANIFEST_MAX_OUTPUT_LIMIT)
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
pub(crate) fn run_manifest_app_task(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
                (1..=MANIFEST_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=MANIFEST_MAX_OUTPUT_LIMIT)
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

pub(crate) fn run_manifest_ensure(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
                (1..=MANIFEST_MAX_DEADLINE_MS)
                    .contains(&value)
                    .then_some(value)
                    .ok_or(())
            }),
            "--output-limit" => set_once_parsed(&mut output_limit, arguments.next(), |value| {
                let value = parse_usize(value)?;
                (1..=MANIFEST_MAX_OUTPUT_LIMIT)
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
