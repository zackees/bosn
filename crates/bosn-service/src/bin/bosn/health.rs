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
pub(crate) fn doctor_unmanaged_warning(state_dir: &Path) {
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
