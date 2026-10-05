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

/// A parsed destructive volume apply. Both flags are required: `apply` states
/// the intent to remove durable data, `yes` confirms it. There is deliberately
/// no bulk mode, no `--force`, and no other predicate-widening flag; the
/// candidate token is the only thing that names what may be removed.
struct VolumeApplyArguments {
    state_dir: PathBuf,
    workspace: PathBuf,
    candidate: String,
    json_output: bool,
}

/// Parse the apply form shared by `manifest volume-gc apply` and
/// `manifest volume-release apply`. Eagerly typed at the boundary so a refused
/// form never reaches the client.
fn parse_volume_apply_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<VolumeApplyArguments, ()> {
    let mut state_dir = None;
    let mut workspace = None;
    let mut candidate = None;
    let mut apply = false;
    let mut yes = false;
    let mut json_output = false;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--workspace" => set_once_parsed(&mut workspace, arguments.next(), parse_state_dir),
            "--candidate" => set_once_parsed(&mut candidate, arguments.next(), |v| {
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
        }?;
    }
    if !apply || !yes {
        return Err(());
    }
    Ok(VolumeApplyArguments {
        state_dir: state_dir.ok_or(())?,
        workspace: workspace.ok_or(())?,
        candidate: candidate.ok_or(())?,
        json_output,
    })
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
        let parsed = parse_volume_apply_arguments(arguments).unwrap_or_else(|_| usage());
        let result = Client::for_state(parsed.state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_release_apply(
                            &parsed.workspace,
                            &parsed.candidate,
                            true,
                        ))
                        .ok()
                })
        });
        match result {
            Some(value) => println!(
                "{}",
                json!({"action":"manifest_volume_release_apply","removed":value.removed,"reconciled_missing":value.reconciled_missing})
            ),
            None => gc_failure(parsed.json_output),
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
        let parsed = parse_volume_apply_arguments(arguments).unwrap_or_else(|_| usage());
        let result = Client::for_state(parsed.state_dir).ok().and_then(|client| {
            RuntimeBuilder::current_thread()
                .enable_all()
                .build()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .run(client.manifest_volume_gc_apply(
                            &parsed.workspace,
                            &parsed.candidate,
                            true,
                        ))
                        .ok()
                })
        });
        match result {
            Some(value) => println!(
                "{}",
                json!({"action":"manifest_volume_gc_apply","removed":value.removed,"reconciled_missing":value.reconciled_missing})
            ),
            None => gc_failure(parsed.json_output),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(values: &[&str]) -> Result<VolumeApplyArguments, ()> {
        parse_volume_apply_arguments(values.iter().map(std::ffi::OsString::from))
    }

    /// The documented durable-volume release form (#524) parses. `docs/
    /// rust-manifest-runtime.md` promises an opaque candidate token plus
    /// `--apply --yes`; this holds the CLI to exactly that shape.
    #[test]
    fn volume_release_apply_parses_the_documented_form() {
        let parsed = apply(&[
            "--state-dir",
            "/state",
            "--workspace",
            "/work",
            "--candidate",
            "VOLUME-RELEASE-TOKEN",
            "--apply",
            "--yes",
            "--json",
        ])
        .unwrap();
        assert_eq!(parsed.state_dir, PathBuf::from("/state"));
        assert_eq!(parsed.workspace, PathBuf::from("/work"));
        assert_eq!(parsed.candidate, "VOLUME-RELEASE-TOKEN");
        assert!(parsed.json_output);
    }

    /// Both confirmations are load-bearing. Dropping either one must refuse
    /// before the client is reached, so apply cannot remove durable data.
    #[test]
    fn apply_refuses_without_both_apply_and_yes() {
        let base = [
            "--state-dir",
            "/state",
            "--workspace",
            "/work",
            "--candidate",
            "T",
        ];
        let mut without_yes = base.to_vec();
        without_yes.push("--apply");
        assert!(apply(&without_yes).is_err());

        let mut without_apply = base.to_vec();
        without_apply.push("--yes");
        assert!(apply(&without_apply).is_err());

        assert!(apply(&base).is_err());
    }

    /// No bulk mode, no `--force`, no other widening flag. Anything unrecognized
    /// is refused rather than ignored.
    #[test]
    fn apply_refuses_unknown_and_repeated_flags() {
        for bad in [
            &["--all"][..],
            &["--force"][..],
            &["--yes"][..],
            &["--workspace"][..],
        ] {
            let mut values = vec![
                "--state-dir",
                "/state",
                "--workspace",
                "/work",
                "--candidate",
                "T",
                "--apply",
                "--yes",
            ];
            values.extend_from_slice(bad);
            assert!(apply(&values).is_err(), "accepted {bad:?}");
        }
    }

    /// Missing required operands are refused, not defaulted.
    #[test]
    fn apply_requires_state_dir_workspace_and_candidate() {
        let full = [
            "--state-dir",
            "/state",
            "--workspace",
            "/work",
            "--candidate",
            "T",
            "--apply",
            "--yes",
        ];
        for drop in ["--state-dir", "--workspace", "--candidate"] {
            let index = full.iter().position(|value| *value == drop).unwrap();
            let mut values = full.to_vec();
            values.drain(index..index + 2);
            assert!(apply(&values).is_err(), "accepted without {drop}");
        }
    }
}
