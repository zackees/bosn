//! `bosn registry`: diagnostics, v4 import and reconciliation.

use super::*;

/// Bounded, read-only daemon diagnostics. These commands deliberately require
/// the already-running daemon: the CLI does not open, create, or migrate a
/// SQLite registry and therefore preserves the daemon's single-writer model.
pub(crate) fn run_registry(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
pub(crate) fn run_registry_import_v4(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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

pub(crate) fn registry_import_failure(json_output: bool) -> ! {
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
pub(crate) fn run_registry_reconcile_v4(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    let is_apply = match verb.to_str() {
        Some("preview") if !apply && !yes => false,
        Some("apply") if apply && yes => true,
        _ => registry_reconcile_failure(json_output),
    };
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

pub(crate) fn registry_reconcile_failure(json_output: bool) -> ! {
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

pub(crate) struct OfflinePythonV4Docker {
    pub(crate) engine: DockerEngine,
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

pub(crate) enum RegistryInvocation {
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
    pub(crate) fn state_dir(&self) -> &std::path::Path {
        match self {
            Self::Resources { state_dir, .. } | Self::SetupEnsureEvents { state_dir, .. } => {
                state_dir
            }
        }
    }
    pub(crate) fn json(&self) -> bool {
        match self {
            Self::Resources { json, .. } | Self::SetupEnsureEvents { json, .. } => *json,
        }
    }
    pub(crate) fn action(&self) -> &'static str {
        match self {
            Self::Resources { .. } => "resources",
            Self::SetupEnsureEvents { .. } => "setup-ensure-events",
        }
    }
}

pub(crate) fn parse_registry_arguments(
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

pub(crate) fn parse_registry_limit(value: std::ffi::OsString) -> Result<u32, ()> {
    let value = parse_u64(value)?;
    (1..=u64::from(bosn_service::MAX_REGISTRY_DIAGNOSTIC_PAGE))
        .contains(&value)
        .then_some(value as u32)
        .ok_or(())
}

pub(crate) fn registry_failure(action: &str, json: bool) -> ! {
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

pub(crate) fn print_registry_resources(
    page: bosn_service::RegistryResourcePage,
    _json_output: bool,
) {
    let records: Vec<_> = page.records.into_iter().map(|record| json!({"id": record.id, "kind": record.kind, "name": record.name, "stack": record.stack, "generation": record.generation, "state": record.state, "retention": record.retention, "created_at": record.created_at, "last_used": record.last_used})).collect();
    let value = json!({"action": "registry_resources", "next": page.next, "records": records});
    println!("{value}");
}
pub(crate) fn print_setup_ensure_events(
    page: bosn_service::SetupEnsureEventPage,
    _json_output: bool,
) {
    let records: Vec<_> = page.records.into_iter().map(|record| json!({"cursor": record.cursor, "at": record.at, "kind": record.kind, "detail": record.detail})).collect();
    let value = json!({"action": "setup_ensure_events", "next": page.next, "records": records});
    println!("{value}");
}
