//! `bosn doctor`.

use super::*;

/// Read the fixed, daemon-owned health report. Argument parsing happens before
/// any runtime/IPC work and deliberately exposes neither Docker controls nor
/// diagnostic output/deadline controls.
pub(crate) fn run_doctor(arguments: impl Iterator<Item = std::ffi::OsString>) {
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
    // #300: the census runs before the report is printed and inside a fixed overall budget,
    // so doctor is one bounded, completed operation that states whether its census finished.
    let scan = doctor_census(&state_dir);
    let value = json!({
        "action": "doctor",
        "daemon": report.daemon,
        "registry": report.registry,
        "engine": report.engine,
        "client_version": report.client_version,
        "server_version": report.server_version,
        "unmanaged_census": census_status(&scan),
        "census_deadline_ms": DOCTOR_CENSUS_BUDGET.as_millis(),
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
    doctor_unmanaged_warning(&state_dir, &scan);
}

/// The whole doctor census budget (#300). The installed-wheel smoke gives doctor 10 s; this
/// leaves room for daemon IPC and process start-up on a slow Windows runner.
pub(crate) const DOCTOR_CENSUS_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// One census pass bounded overall by [`DOCTOR_CENSUS_BUDGET`].
fn doctor_census(state_dir: &Path) -> bosn_service::unmanaged::UnmanagedCensus {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());
    bosn_service::unmanaged::unmanaged_census_until(
        &DockerEngine::docker(),
        our_registry.as_deref(),
        census_config(None),
        census_deadline(None),
        DOCTOR_CENSUS_BUDGET,
    )
}

/// `complete`, `unavailable` (nothing could be read) or `incomplete`. Never "clean".
fn census_status(scan: &bosn_service::unmanaged::UnmanagedCensus) -> &'static str {
    if scan.is_trustworthy() {
        "complete"
    } else if scan.census.classes.is_empty() && !scan.unreadable.is_empty() {
        "unavailable"
    } else {
        "incomplete"
    }
}

/// Print owned-volume and unmanaged-artifact warnings from one census.
///
/// An unreachable engine is reported as unavailable rather than as a warning: crying
/// "not known to be clean" on every machine without Docker would make the loud warning the
/// noise it is meant not to be.
pub(crate) fn doctor_unmanaged_warning(
    state_dir: &Path,
    scan: &bosn_service::unmanaged::UnmanagedCensus,
) {
    let owned = bosn_service::owned_accounting::summarize(&scan.artifacts, scan.census.partial);
    for line in owned.warning_lines(warning_threshold(None, None)) {
        eprintln!("{line}");
    }
    if census_status(scan) == "unavailable" {
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
