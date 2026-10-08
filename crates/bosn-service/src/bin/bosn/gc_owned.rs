//! `bosn gc owned`: age-based reclamation of the resources this registry owns.
//!
//! The unmanaged census deliberately protects everything Bosn owns
//! (`ProtectedReason::OwnedByThisRegistry`), so it can never reclaim a `bosn-setup-v2-*`
//! container, a `bosn-v-*` volume or a `bosn-setup:*` image. Without this command those
//! accumulate without bound, and a stopped container pins every volume it ever mounted. See
//! `docs/rust-managed-retention.md`.
//!
//! Preview is the default and `--apply --yes` is required to remove anything, matching the
//! existing `gc --unmanaged` contract. The daemon re-derives the plan itself and revalidates
//! ownership immediately before each removal, so a stale preview can never widen what a pass
//! deletes.

use bosn_core::retention::{
    DEFAULT_CONTAINER_TTL, DEFAULT_IMAGE_TTL, DEFAULT_VOLUME_TTL, RetentionPolicy,
};

use super::*;

/// Parse the per-kind age gates and the byte ceiling.
///
/// A gate is a duration in seconds. The three gates are independent because the costs differ by
/// orders of magnitude: a setup container is reconstructible in seconds, a volume holds a
/// toolchain, and an image took a full build.
pub(crate) fn parse_gc_owned_arguments(
    mut arguments: impl Iterator<Item = std::ffi::OsString>,
) -> Result<(PathBuf, RetentionPolicy, bool, bool, bool), ()> {
    let mut state_dir = None;
    let mut container_ttl = None;
    let mut volume_ttl = None;
    let mut image_ttl = None;
    let mut max_bytes = None;
    let mut apply = false;
    let mut yes = false;
    let mut json = false;

    while let Some(argument) = arguments.next() {
        match argument.to_string_lossy().as_ref() {
            "--state-dir" => set_once_parsed(&mut state_dir, arguments.next(), parse_state_dir),
            "--container-ttl-secs" => set_once_parsed(
                &mut container_ttl,
                arguments.next(),
                parse_retention_seconds,
            ),
            "--volume-ttl-secs" => {
                set_once_parsed(&mut volume_ttl, arguments.next(), parse_retention_seconds)
            }
            "--image-ttl-secs" => {
                set_once_parsed(&mut image_ttl, arguments.next(), parse_retention_seconds)
            }
            "--max-bytes" => set_once_parsed(&mut max_bytes, arguments.next(), parse_byte_count),
            "--apply" if !apply => {
                apply = true;
                Ok(())
            }
            "--yes" if !yes => {
                yes = true;
                Ok(())
            }
            "--json" if !json => {
                json = true;
                Ok(())
            }
            _ => Err(()),
        }?;
    }

    // Every gate is required. A defaulted gate would silently reclaim under a TTL the operator
    // never chose, which is the opposite of what an explicit command should do.
    Ok((
        state_dir.ok_or(())?,
        RetentionPolicy {
            container_ttl: std::time::Duration::from_secs(container_ttl.ok_or(())?),
            volume_ttl: std::time::Duration::from_secs(volume_ttl.ok_or(())?),
            image_ttl: std::time::Duration::from_secs(image_ttl.ok_or(())?),
            max_bytes,
        },
        apply,
        yes,
        json,
    ))
}

fn parse_retention_seconds(value: std::ffi::OsString) -> Result<u64, ()> {
    value.to_str().ok_or(())?.parse::<u64>().map_err(|_| ())
}

/// Byte counts accept a `k`/`m`/`g` suffix so an operator can write `--max-bytes 50g`.
fn parse_byte_count(value: std::ffi::OsString) -> Result<i128, ()> {
    let raw = value.to_str().ok_or(())?.trim().to_ascii_lowercase();
    let (digits, multiplier) = match raw.strip_suffix('k') {
        Some(rest) => (rest, 1024_i128),
        None => match raw.strip_suffix('m') {
            Some(rest) => (rest, 1024_i128 * 1024),
            None => match raw.strip_suffix('g') {
                Some(rest) => (rest, 1024_i128 * 1024 * 1024),
                None => (raw.as_str(), 1),
            },
        },
    };
    digits
        .parse::<i128>()
        .map(|n| n.saturating_mul(multiplier))
        .map_err(|_| ())
}

/// `bosn gc owned` — preview by default, `--apply --yes` to reclaim.
pub(crate) fn run_gc_owned(arguments: impl Iterator<Item = std::ffi::OsString>) {
    let (state_dir, policy, apply, yes, json_output) =
        parse_gc_owned_arguments(arguments).unwrap_or_else(|_| owned_usage());

    // Both flags are required even though the verb implies destruction, so a bare invocation
    // from shell history or a script can never delete. This mirrors `gc --unmanaged`.
    if apply != yes {
        owned_usage();
    }

    let client = Client::for_state(state_dir).unwrap_or_else(|_| owned_failure(json_output));
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|_| owned_failure(json_output));
    let summary = runtime
        .run(client.managed_retention(policy, apply))
        .unwrap_or_else(|_| owned_failure(json_output));

    println!(
        "{}",
        json!({
            "action": "gc_owned",
            "applied": summary.applied,
            "preview_only": !summary.applied,
            "held": summary.held,
            "held_total": summary.held_total,
            "held_details_omitted": summary.held_total.saturating_sub(summary.held.len() as u64),
            "planned": summary.planned,
            "removed": summary.removed,
            "removed_bytes": summary.removed_bytes,
            "deferred": summary.deferred,
            "failed": summary.failed,
            "failures": summary.failures,
            "refused": summary.refused,
        })
    );
}

fn owned_usage() -> ! {
    eprintln!(
        "bosn gc owned --state-dir DIR --container-ttl-secs N --volume-ttl-secs N \
         --image-ttl-secs N [--max-bytes N] [--apply --yes] [--json]\n\
         \n\
         Preview (default) or reclaim the containers, volumes and images this registry owns.\n\
         Ownership, liveness and pins are rechecked by the daemon immediately before every\n\
         removal. Suggested starting gates: container {}s, volume {}s, image {}s.",
        DEFAULT_CONTAINER_TTL.as_secs(),
        DEFAULT_VOLUME_TTL.as_secs(),
        DEFAULT_IMAGE_TTL.as_secs(),
    );
    std::process::exit(2)
}

pub(crate) fn owned_failure(json: bool) -> ! {
    if json {
        println!(
            "{}",
            json!({"action":"gc_owned","error":"daemon unavailable or request failed"})
        );
    } else {
        eprintln!("bosn gc owned: daemon unavailable or request failed");
    }
    std::process::exit(1)
}
