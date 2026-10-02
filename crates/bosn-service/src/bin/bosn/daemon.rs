//! `bosn daemon` and `bosn mcp`.

use super::*;

/// Run the deliberately small, package-ready foreground daemon surface. It
/// does not fork, register an autostart entry, or make Docker calls. The
/// service itself owns state-directory hardening, registry-writer exclusion,
/// and authenticated local IPC.
/// Register or unregister the daemon with the platform's user service manager.
///
/// Writing the entry file is not the operation — registering it is. The Python
/// implementation wrote a LaunchAgent plist and returned, so a macOS user who asked for
/// autostart got nothing until their next login.
pub(crate) fn run_daemon_autostart(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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

pub(crate) fn run_daemon(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
            if let Err(error) = runtime.run(
                bosn_service::Service::new(state_dir)
                    .with_release_version(env!("CARGO_PKG_VERSION"))
                    .serve(),
            ) {
                eprintln!("bosn daemon serve: {error}");
                std::process::exit(1);
            }
            println!("daemon stopped");
        }
        DaemonInvocation::Status { state_dir, json } => {
            let Ok(client) = Client::for_state(&state_dir) else {
                daemon_failure("status", json)
            };
            // Ask for the version first: it is the one request every daemon
            // release answers, so a mismatch is reported, not a bare failure.
            let daemon_version = runtime.run(client.daemon_version()).ok();
            let status = match runtime.run(client.status()) {
                Ok(status) => status,
                Err(_) => {
                    if let Some(mismatch) = daemon_version.as_deref().and_then(|version| {
                        bosn_service::daemon_version_mismatch(
                            &state_dir,
                            env!("CARGO_PKG_VERSION"),
                            version,
                        )
                    }) && !json
                    {
                        eprintln!("bosn daemon status: {mismatch}");
                    }
                    daemon_failure("status", json)
                }
            };
            print_daemon_status(&status, daemon_version.as_deref(), &state_dir, json);
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

pub(crate) enum DaemonInvocation {
    Serve { state_dir: PathBuf },
    Status { state_dir: PathBuf, json: bool },
    Stop { state_dir: PathBuf, json: bool },
}

impl DaemonInvocation {
    pub(crate) fn action(&self) -> &'static str {
        match self {
            Self::Serve { .. } => "serve",
            Self::Status { .. } => "status",
            Self::Stop { .. } => "stop",
        }
    }

    pub(crate) fn json(&self) -> bool {
        match self {
            Self::Serve { .. } => false,
            Self::Status { json, .. } | Self::Stop { json, .. } => *json,
        }
    }
}

/// Parse every daemon argument before constructing a runtime, asking the
/// client to resolve an endpoint, or allowing `serve` to create state.
pub(crate) fn parse_daemon_serve_arguments(
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

pub(crate) fn parse_daemon_client_arguments(
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

pub(crate) fn daemon_failure(action: &str, json: bool) -> ! {
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

pub(crate) fn print_daemon_status(
    status: &bosn_service::Status,
    daemon_version: Option<&str>,
    state_dir: &Path,
    json: bool,
) {
    let client_version = env!("CARGO_PKG_VERSION");
    let mismatch = daemon_version.and_then(|version| {
        bosn_service::daemon_version_mismatch(state_dir, client_version, version)
    });
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
                "daemon_version": daemon_version.filter(|version| !version.is_empty()),
                "client_version": client_version,
                "version_mismatch": mismatch,
            })
        );
    } else {
        println!("daemon status");
        println!("daemon: online");
        println!(
            "daemon_version: {}",
            daemon_version
                .filter(|version| !version.is_empty())
                .unwrap_or("unknown")
        );
        println!("client_version: {client_version}");
        println!("registry_id: {}", status.registry_id);
        println!("schema_version: {}", status.schema_version);
        println!("resources: {}", status.resources);
        println!("leases: {}", status.leases);
        println!("sessions: {}", status.sessions);
        println!(
            "reconciliation_required: {}",
            status.reconciliation_required
        );
        if let Some(mismatch) = mismatch {
            eprintln!("bosn daemon status: {mismatch}");
        }
    }
}

pub(crate) fn run_mcp(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
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
