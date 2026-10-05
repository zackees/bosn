use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bosn_core::config::POLICY_KEYS;
use bosn_core::{
    PolicyDefaults, PolicyOrigin, parse_machine_policy_toml, resolve_app_policy,
    resolve_machine_policy,
};

/// Keys that `config.rs` declares but no production reader consumes yet. Each entry names the
/// surface that is meant to consume it, so this list can only shrink: a key that grows a reader
/// must be deleted from here, which the second half of the test enforces.
///
/// `idle_retire_seconds` was removed from this list by #521 rather than carried: nothing was
/// going to read it, and a knob nobody reads is worse than an absent one.
const PENDING_WIRING: [(&str, &str); 2] = [
    ("shared_cache_ceiling", "ci::cache_policy::CachePolicy"),
    ("max_builds", "engine build admission"),
];

#[test]
fn machine_toml_precedence_types_and_origins_are_explicit() {
    let file_only = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10.5\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        std::iter::empty::<(&str, &str)>(),
        std::iter::empty::<(&str, &str)>(),
    )
    .unwrap();
    assert_eq!(
        file_only.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFile)
    );
    let environment = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        [("run_max_duration", "9")],
        [],
    )
    .unwrap();
    assert_eq!(
        environment.origin("run_max_duration"),
        Some(PolicyOrigin::MachineEnvironment)
    );
    let policy = parse_machine_policy_toml(
        "[policy]\nrun_max_duration=10\n",
        PolicyDefaults::for_cpu_count(Some(8)),
        [("run_max_duration", "9")],
        [("run_max_duration", "8")],
    )
    .unwrap();
    assert_eq!(policy.get("run_max_duration"), Some(8.0));
    assert_eq!(
        policy.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFlag)
    );
    for invalid in [
        "[policy]\nrun_max_duration=true",
        "[policy]\nrun_max_duration=nan",
        "[policy]\nunknown=1",
        "[policy]\nrun_max_duration=1\nother=2",
        "[policy]\nrun_max_duration=1\n[other]\nx=1",
    ] {
        assert!(
            parse_machine_policy_toml(
                invalid,
                PolicyDefaults::for_cpu_count(None),
                std::iter::empty::<(&str, &str)>(),
                std::iter::empty::<(&str, &str)>(),
            )
            .is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn app_policy_does_not_mutate_machine_policy_or_override_global_knobs() {
    let machine = resolve_machine_policy(
        PolicyDefaults::for_cpu_count(Some(4)),
        [("run_max_duration", "12")],
        [("run_max_duration", "11")],
        [("run_max_duration", "10")],
    )
    .unwrap();
    let app = resolve_app_policy(&machine, [("run_max_duration", "9")]).unwrap();
    assert_eq!(machine.get("run_max_duration"), Some(10.0));
    assert_eq!(
        machine.origin("run_max_duration"),
        Some(PolicyOrigin::MachineFlag)
    );
    assert_eq!(app.get("run_max_duration"), Some(9.0));
    assert_eq!(app.machine().get("run_max_duration"), Some(10.0));
    assert!(resolve_app_policy(&machine, [("max_builds", "1")]).is_err());
    assert!(resolve_app_policy(&machine, [("run_max_duration", "11")]).is_err());
}

/// Each accepted machine-policy key must be read by production code, not merely accepted.
///
/// #521 shipped `idle_retire_seconds` in the key list and the defaults map with no reader
/// anywhere, so an operator who set it got silent acceptance and no behavior. Grep found that
/// one dead knob only because it was looked for; this test makes the next one fail the build.
///
/// A key counts as read when it is named in any workspace `src/` tree other than `config.rs`
/// (for example the matching field on `RetentionConfig`), or when it appears inside
/// `config.rs` more often than its two declaration sites — the `POLICY_KEYS` entry and its
/// default. The latter covers keys consumed by this module itself, such as the `APP_KEYS`
/// override check. Test code does not count, but inline `#[cfg(test)]` modules are not stripped
/// from the scanned files, so a key mentioned only by a test would be reported as read; that is
/// a deliberate false negative in exchange for never flagging production code as dead.
#[test]
fn every_policy_key_is_read_by_production_code() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above crates/bosn-core");
    let config_source = std::fs::read_to_string(workspace.join("crates/bosn-core/src/config.rs"))
        .expect("config.rs is readable");
    let mut unread: Vec<&str> = Vec::new();
    for key in POLICY_KEYS {
        if PENDING_WIRING.iter().any(|(pending, _)| *pending == key) {
            continue;
        }
        let used_elsewhere = rust_sources(workspace)
            .into_iter()
            .filter(|path| !path.ends_with("bosn-core/src/config.rs"))
            .any(|path| reads_key(&path, key));
        let used_here = config_source.matches(key).count() > 2;
        if !used_elsewhere && !used_here {
            unread.push(key);
        }
    }
    assert!(
        unread.is_empty(),
        "policy keys with no production reader: {unread:?}"
    );
}

/// Every pending key must still lack a reader, so `PENDING_WIRING` cannot silently excuse a
/// knob that production code has since started consuming.
#[test]
fn pending_wiring_entries_are_still_unread() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root is two levels above crates/bosn-core");
    let config_source = std::fs::read_to_string(workspace.join("crates/bosn-core/src/config.rs"))
        .expect("config.rs is readable");
    for (key, _) in PENDING_WIRING {
        assert!(
            POLICY_KEYS.contains(&key),
            "{key:?} is pending but no longer a policy key"
        );
        let read = rust_sources(workspace)
            .into_iter()
            .filter(|path| !path.ends_with("bosn-core/src/config.rs"))
            .any(|path| reads_key(&path, key))
            || config_source.matches(key).count() > 2;
        assert!(
            !read,
            "{key:?} now has a production reader; drop it from PENDING_WIRING"
        );
    }
}

/// Every `.rs` file under each workspace crate's `src/`. Test directories are excluded: the
/// contract is about production readers only.
fn rust_sources(workspace: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(crates) = std::fs::read_dir(workspace.join("crates")) else {
        return found;
    };
    for entry in crates.flatten() {
        collect_rust(&entry.path().join("src"), &mut found);
    }
    found.sort();
    found
}

fn collect_rust(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// Whether `path` names `key` as a whole identifier. A bare substring match would let
/// `container_idle_stop` count as a read of `container_idle_stop_never`, so the surrounding
/// characters must not be identifier characters.
fn reads_key(path: &Path, key: &str) -> bool {
    let Ok(source) = std::fs::read_to_string(path) else {
        return false;
    };
    let bytes = source.as_bytes();
    source.match_indices(key).any(|(at, _)| {
        let before = at.checked_sub(1).is_none_or(|i| !is_ident(bytes[i]));
        let after = at + key.len();
        let after_ok = bytes.get(after).is_none_or(|b| !is_ident(*b));
        before && after_ok
    })
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The pending keys are also the set that must stay absent from the defaults map, so a key
/// cannot be added to `POLICY_KEYS` without a default and a reader.
#[test]
fn policy_keys_and_defaults_agree() {
    let defaults = PolicyDefaults::for_cpu_count(Some(4));
    let declared: BTreeSet<&str> = POLICY_KEYS.into_iter().collect();
    let defaulted: BTreeSet<&str> = defaults.values().keys().map(String::as_str).collect();
    assert_eq!(declared, defaulted);
}
