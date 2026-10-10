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

    // Applying requires every gate and the state directory: a defaulted gate would silently
    // reclaim under a TTL the operator never chose. A preview removes nothing, so it defaults to
    // the daemon's own state directory and policy, which makes the bare `bosn gc owned` the
    // maintenance log prints runnable as shown (#551).
    let defaults = RetentionPolicy::default();
    let gate = |value: Option<u64>, default: std::time::Duration| match value {
        Some(secs) => Ok(std::time::Duration::from_secs(secs)),
        None if !apply => Ok(default),
        None => Err(()),
    };
    let state_dir = match state_dir {
        Some(dir) => dir,
        None if !apply => bosn_service::mcp::default_state_dir(),
        None => return Err(()),
    };
    Ok((
        state_dir,
        RetentionPolicy {
            container_ttl: gate(container_ttl, defaults.container_ttl)?,
            volume_ttl: gate(volume_ttl, defaults.volume_ttl)?,
            image_ttl: gate(image_ttl, defaults.image_ttl)?,
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
/// A ceiling must be positive: on the wire `0` means "no ceiling" (#552), so a
/// zero meant as "remove nothing" would otherwise remove without a limit.
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
        .ok()
        .filter(|n| *n > 0)
        .map(|n| n.saturating_mul(multiplier))
        .ok_or(())
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

    require_matching_daemon(&state_dir, "gc owned", json_output);
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

#[cfg(test)]
mod tests {
    use super::{RetentionPolicy, parse_byte_count, parse_gc_owned_arguments};

    fn parse(args: &[&str]) -> Result<RetentionPolicy, ()> {
        parse_gc_owned_arguments(args.iter().map(std::ffi::OsString::from)).map(|parsed| parsed.1)
    }

    /// #551: the maintenance log prints `bosn gc owned`; a preview must run as printed, while
    /// applying still requires every gate and the state directory.
    #[test]
    fn a_preview_defaults_to_the_daemon_policy_but_applying_does_not() {
        assert_eq!(parse(&[]), Ok(RetentionPolicy::default()));
        assert_eq!(parse(&["--json"]), Ok(RetentionPolicy::default()));
        let explicit = parse(&["--container-ttl-secs", "60"]).unwrap();
        assert_eq!(explicit.container_ttl, std::time::Duration::from_secs(60));
        assert_eq!(explicit.volume_ttl, RetentionPolicy::default().volume_ttl);
        let all = [
            "--state-dir",
            "/s",
            "--container-ttl-secs",
            "1",
            "--volume-ttl-secs",
            "2",
            "--image-ttl-secs",
            "3",
            "--apply",
            "--yes",
        ];
        assert!(parse(&all).is_ok());
        for missing in [1, 3, 5, 7] {
            let mut args = all.to_vec();
            args.drain(missing - 1..=missing);
            assert_eq!(parse(&args), Err(()), "{args:?}");
        }
    }

    /// #552: `0` on the wire means "no ceiling", so a zero or negative ceiling
    /// is refused instead of silently removing without a limit.
    #[test]
    fn a_byte_ceiling_must_be_positive() {
        assert_eq!(parse_byte_count("50g".into()), Ok(50 * 1024 * 1024 * 1024));
        assert_eq!(parse_byte_count("1".into()), Ok(1));
        for refused in ["0", "0g", "-5", "", "g", "ten"] {
            assert_eq!(parse_byte_count(refused.into()), Err(()), "{refused}");
        }
    }
}
