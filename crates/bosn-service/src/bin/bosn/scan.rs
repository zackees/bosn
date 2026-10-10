//! `bosn scan`: the unmanaged Docker census.

use super::*;

/// Read-only census of Docker artifacts Bosn does not own.
///
/// This reports; it never deletes, and it never suggests a Docker command. A census that
/// cannot be read completely is reported as partial and is never a clean machine.
pub(crate) fn run_scan(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut state_dir = None;
    let mut ttl_seconds = None;
    let mut warn_bytes = None;
    let mut warn_objects = None;
    let mut json_output = false;
    let mut ack = false;
    let mut deadline = None;
    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--ttl-seconds" => {
                set_once_parsed(&mut ttl_seconds, arguments.next(), parse_ttl_seconds)
            }
            "--warn-bytes" => set_once_parsed(&mut warn_bytes, arguments.next(), parse_ttl_seconds),
            "--warn-objects" => {
                set_once_parsed(&mut warn_objects, arguments.next(), parse_ttl_seconds)
            }
            "--census-deadline-ms" => {
                set_once_parsed(&mut deadline, arguments.next(), parse_census_deadline)
            }
            "--ack" if !ack => {
                ack = true;
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
    let config = census_config(ttl_seconds);
    let threshold = warning_threshold(warn_bytes, warn_objects);
    let (scan, our_registry) = scan_host(&state_dir, config, census_deadline(deadline));
    let census = &scan.census;
    let warning = bosn_core::warning(census, threshold);
    let acknowledged = bosn_core::acknowledgement_suppresses(
        bosn_service::unmanaged::read_acknowledgement(&state_dir),
        census,
        now_seconds(),
    );
    if ack {
        if let Some(warning) = &warning {
            let stored = bosn_core::Acknowledgement {
                at: now_seconds(),
                objects: warning.reclaimable_objects,
                bytes: warning.reclaimable_bytes,
            };
            match bosn_service::unmanaged::write_acknowledgement(&state_dir, stored) {
                Ok(()) => {
                    if !json_output {
                        eprintln!("scan: acknowledged {} objects", stored.objects);
                    }
                }
                Err(detail) => {
                    eprintln!("scan: could not record the acknowledgement: {detail}");
                    std::process::exit(1);
                }
            }
        }
        return;
    }
    let mut owned = bosn_service::owned_accounting::summarize(&scan.artifacts, census.partial);
    let registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3")).ok();
    owned.correlate(registry.as_ref());
    if json_output {
        println!(
            "{}",
            scan_json(scan, warning.as_ref(), acknowledged, &owned)
        );
        return;
    }
    println!("scan");
    print_census(census);
    println!("Bosn storage (Docker approximate sizes):");
    for row in owned.classes {
        let bytes = row
            .approximate_bytes
            .map_or_else(|| "unknown".into(), bosn_service::unmanaged::human_bytes);
        println!(
            "  {}: {} objects, {} attached, {} detached, {bytes}",
            row.class.as_str(),
            row.objects,
            row.attached_objects,
            row.detached_objects
        );
    }
    for detail in &scan.unreadable {
        eprintln!("scan: partial: {detail}");
    }
    // A partial census is never a clean machine, so it warns regardless of size.
    if let Some(warning) = warning
        && !acknowledged
    {
        print_warning(&warning);
    }
    let _ = our_registry;
}

/// The census configuration, with the documented default age gate.
pub(crate) fn census_config(ttl_seconds: Option<f64>) -> bosn_core::CensusConfig {
    bosn_core::CensusConfig {
        ttl_seconds: ttl_seconds.unwrap_or(bosn_core::DEFAULT_TTL_SECONDS),
    }
}

/// `--census-deadline-ms`: a positive per-read deadline for the census.
pub(crate) fn parse_census_deadline(value: std::ffi::OsString) -> Result<std::time::Duration, ()> {
    let millis = parse_u64(value)?;
    (millis > 0)
        .then(|| std::time::Duration::from_millis(millis))
        .ok_or(())
}

/// The per-read census deadline, defaulting to the documented 30 seconds.
pub(crate) fn census_deadline(parsed: Option<std::time::Duration>) -> std::time::Duration {
    parsed.unwrap_or(bosn_service::unmanaged::CENSUS_READ_DEADLINE)
}

pub(crate) fn warning_threshold(
    bytes: Option<f64>,
    objects: Option<f64>,
) -> bosn_core::WarningThreshold {
    let default = bosn_core::WarningThreshold::default();
    bosn_core::WarningThreshold {
        bytes: bytes.map_or(default.bytes, |value| value as i128),
        objects: objects.map_or(default.objects, |value| value as u64),
    }
}

pub(crate) fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// Run one census pass against this host, reading only this registry's UUID from state.
///
/// The registry is opened read-only and only for its identity. A missing or unreadable
/// registry leaves the UUID unknown, which classifies every complete label set as foreign:
/// protective, never exposing.
pub(crate) fn scan_host(
    state_dir: &Path,
    config: bosn_core::CensusConfig,
    deadline: std::time::Duration,
) -> (bosn_service::unmanaged::UnmanagedCensus, Option<String>) {
    let our_registry = bosn_registry::Registry::open_read_only(state_dir.join("registry.sqlite3"))
        .ok()
        .and_then(|registry| registry.registry_id().ok());
    let engine = DockerEngine::docker();
    let scan = bosn_service::unmanaged::unmanaged_census_within(
        &engine,
        our_registry.as_deref(),
        config,
        deadline,
    );
    (scan, our_registry)
}

pub(crate) fn class_tier(tier: bosn_core::Tier) -> &'static str {
    match tier {
        bosn_core::Tier::Reclaimable => "reclaimable",
        bosn_core::Tier::Review => "review",
    }
}

pub(crate) fn print_census(census: &bosn_core::Census) {
    if census.classes.is_empty() {
        println!("nothing unowned observed");
    }
    for summary in &census.classes {
        println!(
            "{:<20} {:<12} {:>6} objects  {}  eligible {}",
            summary.class.as_str(),
            class_tier(summary.tier),
            summary.objects,
            bosn_service::unmanaged::human_bytes(summary.bytes),
            summary.eligible_objects,
        );
    }
    if census.reclaimable_objects > 0 {
        println!(
            "eligible: {} objects, {}",
            census.reclaimable_objects,
            bosn_service::unmanaged::human_bytes(census.reclaimable_bytes)
        );
    }
    for summary in &census.protected {
        println!(
            "protected: {:<26} {:>6} objects  {}",
            summary.reason.as_str(),
            summary.objects,
            bosn_service::unmanaged::human_bytes(summary.bytes)
        );
    }
}

pub(crate) fn scan_json(
    scan: bosn_service::unmanaged::UnmanagedCensus,
    warning: Option<&bosn_core::Warning>,
    acknowledged: bool,
    owned: &bosn_service::owned_accounting::OwnedStorage,
) -> serde_json::Value {
    let census = &scan.census;
    let classes: Vec<_> = census
        .classes
        .iter()
        .map(|summary| {
            json!({
                "class": summary.class.as_str(),
                "tier": class_tier(summary.tier),
                "objects": summary.objects,
                "bytes": summary.bytes,
                "eligible_objects": summary.eligible_objects,
                "eligible_bytes": summary.eligible_bytes,
                "oldest_age_seconds": summary.oldest_age_seconds,
            })
        })
        .collect();
    let protected: Vec<_> = census
        .protected
        .iter()
        .map(|summary| {
            json!({
                "reason": summary.reason.as_str(),
                "objects": summary.objects,
                "bytes": summary.bytes,
            })
        })
        .collect();
    // JSON never carries caps or ANSI, and never a suggested Docker command.
    let foreign_reclaimable = warning.map_or(json!(null), |warning| {
        json!({
            "reclaimable_objects": warning.reclaimable_objects,
            "reclaimable_bytes": warning.reclaimable_bytes,
            "review_objects": warning.review_objects,
            "review_bytes": warning.review_bytes,
            "report_only_bytes": warning.report_only_bytes,
            "partial": warning.partial,
            "acknowledged": acknowledged,
        })
    });
    json!({
        "action": "scan",
        "reclaimable_objects": census.reclaimable_objects,
        "reclaimable_bytes": census.reclaimable_bytes,
        "bytes_approximate": census.bytes_approximate,
        "partial": census.partial,
        "classes": classes,
        "protected": protected,
        "owned_storage": owned,
        "foreign_reclaimable": foreign_reclaimable,
        "unreadable": scan.unreadable,
    })
}

/// Colour is opt-out by environment and by pipe, matching the existing problem-output
/// precedent. Caps survive; escape codes do not.
pub(crate) fn colour_enabled() -> bool {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM")
        .map(|term| term == "dumb")
        .unwrap_or(false)
    {
        return false;
    }
    std::io::stderr().is_terminal()
}

/// The loud warning. Printed to stderr, at most once per invocation, and never in JSON.
///
/// The lines come from the shared formatter so the daemon's unattended pass and this
/// interactive surface say exactly the same thing.
pub(crate) fn print_warning(warning: &bosn_core::Warning) {
    let colour = colour_enabled();
    for (index, line) in bosn_service::unmanaged::warning_lines(warning)
        .iter()
        .enumerate()
    {
        // Only the headline shouts, and only it is coloured: an all-caps table is unreadable
        // and the shouting has to mean something.
        if colour && index == 0 {
            eprintln!("\x1b[33m{line}\x1b[0m");
        } else {
            eprintln!("{line}");
        }
    }
}
